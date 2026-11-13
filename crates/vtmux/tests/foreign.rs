//! vtmux and a real tmux 3.8 server on one socket. Run as `tmux`, vtmux
//! uses tmux's socket directory (D20), so its client can reach a tmux
//! server, and tmux's client a vtmux one. Neither speaks the other's
//! protocol: a vtmux client says which server it met and how out, at once;
//! a tmux client fails as tmux makes it. Neither server is disturbed.
//!
//! Every world has its own `/tmp` root: `TMUX_TMPDIR`, `HOME` and a `bin/`
//! with `tmux` pointing at vtmux, so both binaries resolve `-L` to the
//! same `$TMUX_TMPDIR/tmux-UID/<label>`.
mod common;

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// Only ends a hang: the client that hung before this was fixed waits for
/// ever.
const LIMIT: Duration = Duration::from_secs(20);

fn reference() -> PathBuf {
    let tmux = std::env::var_os("VTMUX_REFERENCE_TMUX").map_or_else(
        || PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache/vtmux/tmux-3.8/bin/tmux"),
        PathBuf::from,
    );
    assert!(tmux.exists(), "reference tmux missing: {}", tmux.display());
    tmux
}

struct World {
    _dir: tempfile::TempDir,
    root: PathBuf,
    label: String,
    /// The server's binary: what tears it down.
    server: PathBuf,
}

impl World {
    /// A server with one detached session `keep`: the reference tmux's,
    /// or vtmux's (run as `tmux`, so on the same socket) when `vtmux`.
    fn new(vtmux: bool) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        // Under /tmp, not $TMPDIR: the socket path must fit `sun_path`.
        let dir = tempfile::Builder::new()
            .prefix("vtf")
            .tempdir_in("/tmp")
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        for d in ["bin", "home", "tt"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_vtmux"), root.join("bin/tmux")).unwrap();
        let server = if vtmux {
            root.join("bin/tmux")
        } else {
            reference()
        };
        let world = Self {
            _dir: dir,
            root,
            label: format!(
                "testF{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            server,
        };
        let out = world.run(&world.server, &[], &["new", "-d", "-s", "keep", "cat", "-"]);
        assert!(out.status.success(), "{out:?}");
        world
    }

    fn env(&self) -> Vec<(&'static str, String)> {
        let path = |p: &str| self.root.join(p).display().to_string();
        vec![
            ("HOME", path("home")),
            ("TMUX_TMPDIR", path("tt")),
            ("PATH", "/usr/bin:/bin".into()),
            ("SHELL", "/bin/sh".into()),
            ("TERM", "xterm-256color".into()),
        ]
    }

    /// `bin -L <label> -f /dev/null args`, `extra` added to the
    /// environment.
    fn command(&self, bin: &Path, extra: Env, args: &[&str]) -> Command {
        let mut cmd = Command::new(bin);
        cmd.args(["-L", &self.label, "-f", "/dev/null"])
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .envs(self.env())
            .envs(extra.iter().copied());
        cmd
    }

    /// [`World::command`]'s output; it must end within [`LIMIT`].
    fn run(&self, bin: &Path, extra: Env, args: &[&str]) -> Output {
        common::output_within(&mut self.command(bin, extra, args), LIMIT)
            .unwrap_or_else(|| panic!("{}: {args:?} hung", bin.display()))
    }

    /// vtmux run as `tmux`, as a user's symlink runs it.
    fn vtmux_as_tmux(&self) -> PathBuf {
        self.root.join("bin/tmux")
    }

    /// The socket both binaries resolve `-L <label>` to.
    fn socket(&self) -> PathBuf {
        self.root
            .join("tt")
            .join(format!("tmux-{}", rustix::process::getuid().as_raw()))
            .join(&self.label)
    }

    /// The server still serves its one session.
    fn assert_undisturbed(&self) {
        let out = self.run(&self.server, &[], &["ls", "-F", "#{session_name}"]);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "keep\n",
            "{}: {out:?}",
            self.server.display()
        );
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let server = self.server.clone();
        common::teardown(&self.root.join("tt"), &self.label, None, |args| {
            self.command(&server, &[], args)
        });
    }
}

/// Variables a client gets besides [`World::env`].
type Env<'a> = &'a [(&'a str, &'a str)];

/// `tmux attach` under an 80x24 PTY: what the terminal showed and the exit
/// status, the client killed if it has not ended within [`LIMIT`].
fn attach(w: &World) -> (String, Option<u32>) {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(w.vtmux_as_tmux());
    cmd.args(["-L", &w.label, "attach"]);
    cmd.cwd(&w.root);
    cmd.env_clear();
    for (k, v) in w.env() {
        cmd.env(k, v);
    }
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let shown = std::thread::spawn(move || {
        let mut all = Vec::new();
        let mut buf = [0; 4096];
        while let Ok(n) = reader.read(&mut buf)
            && n > 0
        {
            all.extend_from_slice(&buf[..n]);
        }
        all
    });
    let end = Instant::now() + LIMIT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status.exit_code());
        }
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(pair.master);
    let shown = String::from_utf8_lossy(&shown.join().unwrap()).into_owned();
    (shown, status)
}

/// vtmux, linked as `tmux`, meets a tmux 3.8 server on tmux's socket: every
/// kind of client says it is not a vtmux server, names the socket and the
/// ways out, and exits 1 at once, whatever the server made of its `Hello`
/// (a small one it waits on for ever, a large one it drops unanswered).
/// The tmux server keeps its session.
#[test]
fn a_vtmux_client_meeting_a_tmux_server_says_so_and_exits() {
    let w = World::new(false);
    let want = format!(
        "not a vtmux server: {} belongs to a tmux server; stop it with the real tmux \
         (its kill-server), or run vtmux as vtmux or with -L <name>\n",
        w.socket().display()
    );
    let big = "x".repeat(8192);
    let cases: [(Env, &[&str]); 5] = [
        (&[], &["ls"]),
        (&[("BIG", &big)], &["ls"]),
        (&[], &["new", "-d", "cat", "-"]),
        (&[], &["kill-server"]),
        (&[], &["-C", "attach"]),
    ];
    for (extra, args) in cases {
        let out = w.run(&w.vtmux_as_tmux(), extra, args);
        let ctx = format!("{args:?} with {} more variables", extra.len());
        assert_eq!(String::from_utf8_lossy(&out.stderr), want, "{ctx}");
        assert_eq!(out.stdout, b"", "{ctx}");
        assert_eq!(out.status.code(), Some(1), "{ctx}");
    }
    // A user's `tmux attach` in a terminal: the line, not a hang.
    let (shown, status) = attach(&w);
    assert_eq!(status, Some(1), "attach: {shown:?}");
    assert!(shown.contains(want.trim_end()), "attach showed {shown:?}");
    w.assert_undisturbed();
}

/// tmux 3.8's client meets a vtmux server: tmux's code fails it as it
/// does (it never gets an answer it reads), within moments, and the vtmux
/// server keeps serving, its session alone, no command of tmux's run.
#[test]
fn a_tmux_client_meeting_a_vtmux_server_leaves_it_serving() {
    let w = World::new(true);
    let tmux = reference();
    for args in [
        &["ls"][..],
        &["new", "-d", "-s", "other", "cat", "-"],
        &["kill-server"],
        &["-C", "attach"],
    ] {
        let out = w.run(&tmux, &[], args);
        assert!(!out.status.success(), "{args:?}: {out:?}");
        w.assert_undisturbed();
    }
    let messages = w.run(&w.server, &[], &["show-messages"]);
    let messages = String::from_utf8_lossy(&messages.stdout);
    for theirs in ["other", "kill-server", "attach"] {
        assert!(!messages.contains(theirs), "{theirs}: {messages}");
    }
}

/// The server's pid, stopped (SIGSTOP) until dropped (SIGCONT), so a
/// failing test never leaves it stopped.
struct Stopped(rustix::process::Pid);

impl Stopped {
    fn new(w: &World) -> Self {
        let out = w.run(&w.server, &[], &["display", "-p", "#{pid}"]);
        let pid: i32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
        let pid = rustix::process::Pid::from_raw(pid).unwrap();
        rustix::process::kill_process(pid, rustix::process::Signal::STOP).unwrap();
        Self(pid)
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        let _ = rustix::process::kill_process(self.0, rustix::process::Signal::CONT);
    }
}

/// `args` run by vtmux as `tmux` in the background: its output once it
/// ends (within [`LIMIT`]).
fn in_background(w: &World, args: &[&str]) -> std::thread::JoinHandle<Option<Output>> {
    let mut cmd = w.command(&w.vtmux_as_tmux(), &[], args);
    std::thread::spawn(move || common::output_within(&mut cmd, LIMIT))
}

/// A vtmux server that says nothing for longer than the client's 200 ms
/// (a command that waits, a server stopped for a while, also past the
/// probe's 2 s) is still a vtmux server to its client: the probe is
/// dropped or gives up, and the client gets the server's real answer.
#[test]
fn a_vtmux_server_slow_to_answer_is_never_taken_for_tmux() {
    let w = World::new(true);
    // A command that waits for a second (`wait-for`), then is let go.
    let waiting = in_background(&w, &["wait-for", "go"]);
    std::thread::sleep(Duration::from_secs(1));
    let out = w.run(&w.server, &[], &["wait-for", "-S", "go"]);
    assert!(out.status.success(), "{out:?}");
    let out = waiting
        .join()
        .unwrap()
        .expect("wait-for ends once signalled");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "", "wait-for");
    assert_eq!(out.status.code(), Some(0), "wait-for");
    // A server stopped while a client connects: for 1 s (it drops the
    // probe once it runs again) and for 3 s (the probe gives up first).
    for secs in [1, 3] {
        let stopped = Stopped::new(&w);
        let ls = in_background(&w, &["ls", "-F", "#{session_name}"]);
        std::thread::sleep(Duration::from_secs(secs));
        drop(stopped);
        let out = ls.join().unwrap().expect("ls ends once the server runs");
        let ctx = format!("ls, server stopped {secs} s");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "", "{ctx}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "keep\n", "{ctx}");
        assert_eq!(out.status.code(), Some(0), "{ctx}");
    }
    w.assert_undisturbed();
}
