//! `vtmux-persist` checkpoints written by a real `target/debug/vtmux`
//! server (D27 phase 3, docs/specs/2026-11-06-live-upgrade.md, decisions
//! 8, 16, 17 and 20). vtmux only: tmux has no such option.
//!
//! Isolation: environment cleared, a private `TMUX_TMPDIR`, `HOME` and
//! `XDG_STATE_HOME`, an explicit `-L`, `-f /dev/null`, pane commands of
//! two words at least. Every wait is for a state, never a fixed time.

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use vtmux_core::protocol;
use vtmux_server::persist;
use vtmux_snapshot as snap;

struct World {
    dir: tempfile::TempDir,
    /// The socket: `-L <label>`, or `-S <path>`.
    socket: [String; 2],
    /// The socket file's name: the start of the state directory's.
    name: String,
    /// Extra environment for every call (the server inherits the first's).
    env: Vec<(&'static str, String)>,
    /// `-f`.
    config: PathBuf,
    pid: Option<rustix::process::Pid>,
}

impl World {
    /// A server with session `a` running `argv`, its checkpoints in
    /// `$XDG_STATE_HOME/vtmux/<label>-<hash>`.
    fn start(argv: &[&str], env: &[(&'static str, &str)]) -> Self {
        Self::start_with(argv, env, None, None)
    }

    /// [`World::start`] on the socket `-S socket` (else a fresh `-L`),
    /// with `config` as the config file (else `/dev/null`).
    fn start_with(
        argv: &[&str],
        env: &[(&'static str, &str)],
        socket: Option<&Path>,
        config: Option<&str>,
    ) -> Self {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = tempfile::tempdir().expect("tempdir");
        let label = format!(
            "cli{}ps{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let (socket, name) = match socket {
            Some(p) => (
                ["-S".to_owned(), p.to_str().unwrap().to_owned()],
                p.file_name().unwrap().to_str().unwrap().to_owned(),
            ),
            None => (["-L".to_owned(), label.clone()], label),
        };
        let config = match config {
            Some(text) => {
                let path = dir.path().join("conf");
                std::fs::write(&path, text).unwrap();
                path
            }
            None => PathBuf::from("/dev/null"),
        };
        let mut w = Self {
            dir,
            socket,
            name,
            env: env.iter().map(|&(k, v)| (k, v.to_owned())).collect(),
            config,
            pid: None,
        };
        let mut new = vec!["new", "-d", "-s", "a", "-n", "w0", "-x", "80", "-y", "24"];
        new.extend_from_slice(argv);
        w.ok(&new);
        w.pid = common::parse_pid(&w.ok(&["display", "-p", "#{pid}"]));
        w
    }

    fn run(&self, args: &[&str]) -> (String, String, i32) {
        let out = Command::new(env!("CARGO_BIN_EXE_vtmux"))
            .args(&self.socket)
            .arg("-f")
            .arg(&self.config)
            .args(args)
            .current_dir(self.dir.path())
            .env_clear()
            .env("HOME", self.dir.path())
            .env("TMUX_TMPDIR", self.dir.path())
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color")
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .output()
            .expect("run vtmux");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.code().unwrap_or(-1),
        )
    }

    /// `args`, which must succeed; its stdout.
    fn ok(&self, args: &[&str]) -> String {
        let (out, err, code) = self.run(args);
        assert_eq!(code, 0, "{args:?}: {err}");
        out
    }

    /// `$XDG_STATE_HOME/vtmux`.
    fn states(&self) -> PathBuf {
        let xdg = self
            .env
            .iter()
            .find(|(k, _)| *k == "XDG_STATE_HOME")
            .map_or_else(|| self.dir.path().join("state"), |(_, v)| PathBuf::from(v));
        xdg.join("vtmux")
    }

    /// This server's checkpoint directory: the one directory in
    /// [`World::states`] named for its socket (`<name>-<hash>`), or where
    /// it will be.
    fn state(&self) -> PathBuf {
        let ours = state_dirs(&self.states(), &self.name);
        assert!(ours.len() <= 1, "{ours:?}");
        ours.into_iter()
            .next()
            .unwrap_or_else(|| self.states().join(format!("{}-unwritten", self.name)))
    }

    /// Wait for the server process to be gone.
    fn gone(&self) {
        let pid = self.pid.expect("server pid");
        wait_until("the server exits", || {
            rustix::process::test_kill_process(pid).is_err()
        });
    }

    /// Wait for the first checkpoint; its manifest.
    fn state_when_written(&self) -> PathBuf {
        wait_until("a checkpoint", || {
            state_dirs(&self.states(), &self.name)
                .iter()
                .any(|d| d.join(persist::MANIFEST).is_file())
        });
        self.state().join(persist::MANIFEST)
    }

    fn kill_server(&self) {
        self.ok(&["kill-server"]);
        self.gone();
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = self.run(&["kill-server"]);
        if let Some(pid) = self.pid {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
        common::reap(self.pid);
    }
}

/// The directories in `states` for a socket named `name`: `name`, or
/// `name-` and a hash.
fn state_dirs(states: &Path, name: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(states) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let leaf = p.file_name().unwrap().to_str().unwrap();
            leaf == name || leaf.strip_prefix(name).is_some_and(|r| r.starts_with('-'))
        })
        .collect();
    out.sort();
    out
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

fn session_names(s: &snap::Snapshot) -> Vec<&str> {
    s.sessions.iter().map(|x| x.name.as_str()).collect()
}

/// A pane that prints [`SECRET`] from two halves, so no command line (the
/// pane's start command, the message log) holds it whole, then idles.
const PRINTS_SECRET: [&str; 3] = ["sh", "-c", "printf '%s%s\\n' SECRET- 4242; exec cat -"];
const SECRET: &str = "SECRET-4242";

/// `text` as the snapshot row codec writes a run of plain cells: each
/// cell's codepoint shifted past its width bits, as a LEB128 varint
/// (`vtmux-snapshot` `row.rs`, `text_of`).
fn row_encoded(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for c in text.chars() {
        let mut v = u64::from(c) << 2;
        loop {
            let byte = u8::try_from(v & 0x7f).unwrap();
            v >>= 7;
            if v == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
    }
    out
}

/// Every file under `dir`, recursively.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(files(&p));
        } else {
            out.push(p);
        }
    }
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Whether any row the snapshot holds reads [`SECRET`].
fn shows_secret(s: &snap::Snapshot) -> bool {
    let text = |r: &snap::Row| -> String {
        r.cells
            .iter()
            .filter_map(|c| char::from_u32(c.codepoint))
            .collect()
    };
    let mut found = false;
    for p in &s.panes {
        for m in &p.modes {
            if let snap::PaneMode::Copy(c) | snap::PaneMode::View(c) = m {
                found |= c.rows.iter().any(|r| text(r).contains(SECRET));
            }
        }
        if let Some(t) = &p.term {
            for screen in std::iter::once(&t.primary).chain(&t.alternate) {
                found |= screen.rows.iter().any(|r| text(r).contains(SECRET));
            }
        }
    }
    for page in s.pages() {
        page.each_row(|r| found |= text(r).contains(SECRET))
            .unwrap();
    }
    found
}

#[test]
fn persist_off_writes_nothing() {
    let w = World::start(&["cat", "-"], &[]);
    w.ok(&["new-window", "-d", "cat", "-"]);
    w.kill_server();
    assert!(!w.dir.path().join("state").exists());
}

/// `kill-server` writes the server as it was, into a directory only the
/// user can read (decisions 8, 16, 17).
#[test]
fn kill_server_checkpoints_into_a_private_directory() {
    let w = World::start(&["cat", "-"], &[]);
    w.ok(&["set", "-s", "vtmux-persist", "layout"]);
    w.ok(&["new-window", "-d", "-n", "w1", "cat", "-"]);
    w.ok(&["new", "-d", "-s", "b", "cat", "-"]);
    w.kill_server();
    let dir = w.state();
    let (s, from) = persist::load(&dir).unwrap();
    assert_eq!(from, persist::MANIFEST);
    assert_eq!(s.meta.purpose, snap::Purpose::CheckpointLayout);
    assert_eq!(session_names(&s), ["a", "b"]);
    assert_eq!(s.windows.len(), 3);
    assert_eq!(s.panes.len(), 3);
    assert!(s.clients.is_empty() && s.descriptors.is_empty());
    for d in [&dir, &dir.join(persist::CHUNKS), dir.parent().unwrap()] {
        assert_eq!(mode(d), 0o700, "{}", d.display());
    }
    for f in files(&dir) {
        assert_eq!(mode(&f), 0o600, "{}", f.display());
    }
}

/// User content planted where a checkpoint might pick it up, each text
/// in one place only: `(text, where, written by full)`. `layout` writes
/// none of it (decision 17); `full` writes the screens and the buffers,
/// never the environment, the message log or prompt history.
const PLANTED: [(&str, &str, bool); 7] = [
    (SECRET, "the screen, and a buffer copied from it", true),
    ("ENVSECRET123", "the global environment", false),
    ("SENDKEYSSECRET", "send-keys text (on the screen too)", true),
    ("SETBUFSECRET", "set-buffer", true),
    ("PROMPTSECRET", "prompt history", false),
    ("LOGSECRET", "the message log alone", false),
    ("SEARCHSECRET", "a copy-mode search", true),
];

/// `layout` writes no user content (decision 17): grep the directory for
/// [`PLANTED`]: a pane's text (on its screen, frozen in copy mode, copied
/// into a buffer), the environment, `send-keys` and `set-buffer` text,
/// prompt history, a search and the message log. `full` is the control:
/// its screens and buffers are found, so the search can find them; what
/// no mode writes is not.
#[test]
fn a_layout_checkpoint_on_disk_holds_no_pane_text() {
    for (setting, full) in [("layout", false), ("full", true)] {
        let tmp = tempfile::tempdir().unwrap();
        let history = tmp.path().join("history");
        std::fs::write(&history, "command:display PROMPTSECRET\n").unwrap();
        let config = format!("set -g history-file '{}'\n", history.display());
        let w = World::start_with(
            &PRINTS_SECRET,
            &[("MY_API_TOKEN", "ENVSECRET123")],
            None,
            Some(&config),
        );
        w.ok(&["set", "-s", "vtmux-persist", setting]);
        wait_until("the pane shows the text", || {
            w.ok(&["capture-pane", "-p", "-t", "a"]).contains(SECRET)
        });
        w.ok(&[
            "send-keys",
            "-t",
            "a",
            "export PASSWORD=SENDKEYSSECRET",
            "Enter",
        ]);
        wait_until("cat echoes the keys", || {
            w.ok(&["capture-pane", "-p", "-t", "a"])
                .matches("SENDKEYSSECRET")
                .count()
                == 2
        });
        // A search, then the screen's first line copied into a buffer.
        w.ok(&["copy-mode", "-t", "a"]);
        w.ok(&[
            "send-keys",
            "-t",
            "a",
            "-X",
            "search-backward",
            "SEARCHSECRET",
        ]);
        w.ok(&["send-keys", "-t", "a", "-X", "history-top"]);
        w.ok(&["send-keys", "-t", "a", "-X", "select-line"]);
        w.ok(&["send-keys", "-t", "a", "-X", "copy-selection-and-cancel"]);
        w.ok(&["set-buffer", "-b", "typed", "SETBUFSECRET"]);
        w.ok(&["display", "-p", "LOGSECRET"]);
        // Every one is there to be written.
        assert!(w.ok(&["show-buffer"]).contains(SECRET));
        assert!(w.ok(&["show-environment", "-g"]).contains("ENVSECRET123"));
        assert!(w.ok(&["show-prompt-history"]).contains("PROMPTSECRET"));
        assert!(w.ok(&["show-messages"]).contains("LOGSECRET"));
        assert!(
            w.ok(&["display", "-p", "-t", "a", "#{pane_search_string}"])
                .contains("SEARCHSECRET")
        );
        assert!(w.ok(&["list-buffers"]).contains("SETBUFSECRET"));
        // The screen frozen in copy mode (`full`'s screens until pane
        // terminal state is carried).
        w.ok(&["copy-mode", "-t", "a"]);
        w.kill_server();
        let dir = w.state();
        let (s, _) = persist::load(&dir).unwrap();
        assert_eq!(session_names(&s), ["a"], "{setting}");
        assert_eq!(shows_secret(&s), full, "{setting}");
        let bytes: Vec<Vec<u8>> = files(&dir)
            .iter()
            .map(|f| std::fs::read(f).unwrap())
            .collect();
        let found = |text: &str| {
            let encoded = row_encoded(text);
            bytes
                .iter()
                .any(|b| contains(b, text.as_bytes()) || contains(b, &encoded))
        };
        for (text, place, by_full) in PLANTED {
            assert_eq!(found(text), full && by_full, "{setting}: {text} ({place})");
        }
        // Copied text is in the buffer, not only in copy mode.
        if full {
            assert!(
                s.server
                    .buffers
                    .iter()
                    .any(|b| contains(&b.data, SECRET.as_bytes()))
            );
        }
    }
}

/// Only the server's structure counts as a change: an idle server that
/// tools poll with read-only commands (each logged in the message log,
/// each a client) writes nothing, not even at shutdown.
#[test]
fn a_polled_idle_server_writes_nothing() {
    let w = World::start(&["cat", "-"], &[("VTMUX_PERSIST_INTERVAL_MS", "50")]);
    w.ok(&["select-pane", "-t", "a", "-T", "pinned"]);
    w.ok(&["set", "-s", "vtmux-persist", "layout"]);
    let manifest = w.state_when_written();
    let first = std::fs::read(&manifest).unwrap();
    for _ in 0..3 {
        for args in [
            &["list-sessions"][..],
            &["list-windows", "-a"],
            &[
                "list-panes",
                "-a",
                "-F",
                "#{pane_id} #{pane_current_command}",
            ],
            &["display", "-p", "#{session_name} #{window_activity}"],
            &["has-session", "-t", "a"],
            &["show-options", "-g"],
            &["list-buffers"],
            &["list-clients"],
            &["show-messages"],
            &["capture-pane", "-p", "-t", "a"],
        ] {
            w.ok(args);
        }
    }
    w.kill_server();
    let now = std::fs::read(&manifest).unwrap();
    if now != first {
        let a = format!("{:#?}", snap::decode(&first).unwrap());
        let b = format!("{:#?}", snap::decode(&now).unwrap());
        for (x, y) in a.lines().zip(b.lines()) {
            if x != y {
                eprintln!("DIFF {x} | {y}");
            }
        }
    }
    assert!(now == first, "rewritten");
    assert!(!manifest.with_file_name(persist::MANIFEST_PREV).exists());
}

/// Sockets of one name in two directories are two servers: two
/// checkpoint directories, neither overwriting the other.
#[test]
fn sockets_of_one_name_keep_separate_checkpoints() {
    let shared = tempfile::tempdir().unwrap();
    let states = shared.path().join("state");
    let worlds: Vec<World> = ["alpha", "beta"]
        .iter()
        .map(|name| {
            let sock_dir = shared.path().join(name);
            std::fs::create_dir(&sock_dir).unwrap();
            let w = World::start_with(
                &["cat", "-"],
                &[("XDG_STATE_HOME", states.to_str().unwrap())],
                Some(&sock_dir.join("rvsock")),
                None,
            );
            w.ok(&["set", "-s", "vtmux-persist", "layout"]);
            w.ok(&["rename-session", "-t", "a", name]);
            w
        })
        .collect();
    for w in &worlds {
        w.kill_server();
    }
    let dirs = state_dirs(&states.join("vtmux"), "rvsock");
    assert_eq!(dirs.len(), 2, "{dirs:?}");
    let mut names: Vec<String> = dirs
        .iter()
        .map(|d| {
            let (s, from) = persist::load(d).unwrap();
            assert_eq!(from, persist::MANIFEST);
            assert!(!d.join(persist::MANIFEST_PREV).exists(), "{}", d.display());
            session_names(&s).join(",")
        })
        .collect();
    names.sort();
    assert_eq!(names, ["alpha", "beta"]);
}

/// A second server on the same socket path (the first's socket file
/// removed under it) finds the checkpoint directory locked: it says so
/// in its message log and writes nothing; the first's checkpoints stand.
#[test]
fn a_second_server_on_a_locked_directory_writes_nothing() {
    let shared = tempfile::tempdir().unwrap();
    let socket = shared.path().join("rvlock");
    let env = [("VTMUX_PERSIST_INTERVAL_MS", "50")];
    let first = World::start_with(&["cat", "-"], &env, Some(&socket), None);
    first.ok(&["set", "-s", "vtmux-persist", "layout"]);
    first.ok(&["rename-window", "-t", "a:0", "first"]);
    let manifest = first.state_when_written();
    std::fs::remove_file(&socket).unwrap();
    let mut env2 = env.to_vec();
    let xdg = first.dir.path().join("state");
    env2.push(("XDG_STATE_HOME", xdg.to_str().unwrap()));
    let second = World::start_with(&["cat", "-"], &env2, Some(&socket), None);
    second.ok(&["set", "-s", "vtmux-persist", "layout"]);
    second.ok(&["rename-window", "-t", "a:0", "second"]);
    wait_until("the second server reports the lock", || {
        second
            .ok(&["show-messages"])
            .contains("in use by another server")
    });
    second.kill_server();
    let (s, _) = persist::load(manifest.parent().unwrap()).unwrap();
    assert_eq!(s.windows[0].name, "first");
    assert_eq!(state_dirs(&first.states(), "rvlock").len(), 1);
    rustix::process::kill_process(first.pid.unwrap(), rustix::process::Signal::TERM).unwrap();
    first.gone();
    let (s, _) = persist::load(manifest.parent().unwrap()).unwrap();
    assert_eq!(s.windows[0].name, "first");
}

/// SIGTERM is a clean shutdown too (`server_signal`, server.c:442-446).
#[test]
fn sigterm_checkpoints() {
    let w = World::start(&["cat", "-"], &[]);
    w.ok(&["set", "-s", "vtmux-persist", "layout"]);
    w.ok(&["rename-window", "-t", "a:0", "before-term"]);
    rustix::process::kill_process(w.pid.unwrap(), rustix::process::Signal::TERM).unwrap();
    w.gone();
    let (s, _) = persist::load(&w.state()).unwrap();
    assert_eq!(s.windows[0].name, "before-term");
}

/// The last session gone ends the server (`exit-empty`): its checkpoint
/// is the empty server (decision 16), the previous one kept.
#[test]
fn the_last_session_gone_checkpoints() {
    let w = World::start(&["cat", "-"], &[("VTMUX_PERSIST_INTERVAL_MS", "50")]);
    w.ok(&["set", "-s", "vtmux-persist", "layout"]);
    wait_until("a periodic checkpoint", || {
        w.state().join(persist::MANIFEST).is_file()
    });
    w.ok(&["kill-session", "-t", "a"]);
    w.gone();
    let (s, _) = persist::load(&w.state()).unwrap();
    assert!(s.sessions.is_empty(), "{:?}", session_names(&s));
    let prev = std::fs::read(w.state().join(persist::MANIFEST_PREV)).unwrap();
    assert!(snap::page_refs(&prev).is_ok());
}

/// While something changes, a checkpoint every interval (60 s; a debug
/// build's `VTMUX_PERSIST_INTERVAL_MS` here), the previous one kept.
#[test]
fn a_changed_server_is_checkpointed_on_its_interval() {
    let w = World::start(&["cat", "-"], &[("VTMUX_PERSIST_INTERVAL_MS", "50")]);
    w.ok(&["set", "-s", "vtmux-persist", "layout"]);
    let manifest = w.state_when_written();
    w.ok(&["new-window", "-d", "-n", "later", "cat", "-"]);
    wait_until("a checkpoint with the new window", || {
        persist::load(&w.state()).is_ok_and(|(s, _)| s.windows.iter().any(|x| x.name == "later"))
    });
    assert!(w.state().join(persist::MANIFEST_PREV).is_file());
    // Turned off: no more checkpoints, not even at exit; the files stay.
    w.ok(&["set", "-s", "vtmux-persist", "off"]);
    let last = std::fs::read(&manifest).unwrap();
    w.ok(&["new-window", "-d", "-n", "unsaved", "cat", "-"]);
    w.kill_server();
    assert_eq!(std::fs::read(&manifest).unwrap(), last);
}

// ------------------------------------------------------------- restoring
//
// A server starting on a socket with a checkpoint restores it (decisions
// 15, 27, 28 and 30; *Design > Persistence > Restoring*).

impl World {
    /// SIGKILL the server: no clean shutdown, no checkpoint of its own.
    fn kill_9(&self) {
        rustix::process::kill_process(self.pid.expect("server pid"), rustix::process::Signal::KILL)
            .unwrap();
        self.gone();
    }

    /// The server running now (after a restart), for `gone` and the
    /// teardown.
    fn repid(&mut self) {
        self.pid = common::parse_pid(&self.ok(&["display", "-p", "#{pid}"]));
    }

    /// The checkpoint on disk holds `@saved` (set last), so it holds
    /// everything done before it too.
    fn wait_saved(&self) {
        self.ok(&["set", "-g", "@saved", "yes"]);
        wait_until("a checkpoint with @saved", || {
            persist::load(&self.state())
                .is_ok_and(|(s, _)| s.server.session_options.iter().any(|o| o.name == "@saved"))
        });
    }
}

/// An attached client under a PTY; what it was sent, as it comes.
struct Attached {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    bytes: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Attached {
    fn new(w: &World, args: &[&str]) -> Self {
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};
        use std::io::Read as _;
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_vtmux"));
        cmd.args(&w.socket);
        cmd.arg("-f");
        cmd.arg(&w.config);
        cmd.args(args);
        cmd.cwd(w.dir.path());
        cmd.env_clear();
        cmd.env("HOME", w.dir.path());
        cmd.env("TMUX_TMPDIR", w.dir.path());
        cmd.env("XDG_STATE_HOME", w.dir.path().join("state"));
        cmd.env("SHELL", "/bin/sh");
        cmd.env("TERM", "xterm-256color");
        cmd.env("PATH", std::env::var_os("PATH").unwrap_or_default());
        for (k, v) in &w.env {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&bytes);
        std::thread::spawn(move || {
            let mut buf = [0; 8192];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Self {
            child,
            bytes,
            _master: pair.master,
        }
    }

    /// The screen its output draws.
    fn screen(&self) -> String {
        let mut t = vtmux_vt::PaneTerm::new(80, 24, 0);
        t.feed(&self.bytes.lock().unwrap());
        String::from_utf8_lossy(&vtmux_vt::capture(&t, vtmux_vt::CaptureOpts::default()))
            .into_owned()
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What a restore must bring back as it was: sessions, windows (names,
/// indexes, layout, active), panes (ids, start command and path, size),
/// user options.
fn structure(w: &World) -> Vec<String> {
    [
        &["list-sessions", "-F", "#{session_id} #{session_name} #{session_windows} #{session_created}"][..],
        &[
            "list-windows",
            "-a",
            "-F",
            "#{session_name}:#{window_index} #{window_id} #{window_name} #{window_active} #{window_layout}",
        ],
        &[
            "list-panes",
            "-a",
            "-F",
            "#{session_name}:#{window_index}.#{pane_index} #{pane_id} #{pane_active} [#{pane_start_command}] #{pane_start_path}",
        ],
        &["show-options", "-v", "-t", "a", "@colour"],
        &["show-options", "-wv", "-t", "a:w1", "@side"],
    ]
    .iter()
    .map(|args| w.ok(args))
    .collect()
}

/// `layout` checkpoint, `kill -9`, and a server started again restores
/// it: the same sessions, windows, layout, ids and user options; every
/// pane dead (status unknown) with its start command and cwd and the dead
/// line on its screen. `list-sessions` does not start the server; any
/// command that starts one restores first (decision 28).
#[test]
fn a_checkpoint_comes_back_after_kill_9() {
    let mut w = World::start(&["cat", "-"], &[("VTMUX_PERSIST_INTERVAL_MS", "50")]);
    let here = w.dir.path().to_str().unwrap().to_owned();
    w.ok(&["set", "-s", "vtmux-persist", "layout"]);
    w.ok(&["split-window", "-h", "-t", "a:w0", "-c", "/", "cat", "-"]);
    w.ok(&["resize-pane", "-t", "a:w0.0", "-x", "30"]);
    w.ok(&[
        "new-window",
        "-d",
        "-t",
        "a:5",
        "-n",
        "w1",
        "-c",
        &here,
        "sh",
        "-c",
        "exec cat -",
    ]);
    w.ok(&["split-window", "-v", "-t", "a:w1", "cat", "-"]);
    w.ok(&["new", "-d", "-s", "b", "-n", "bw", "cat", "-"]);
    w.ok(&["set", "-t", "a", "@colour", "teal"]);
    w.ok(&["set", "-w", "-t", "a:w1", "@side", "left"]);
    let before = structure(&w);
    w.wait_saved();
    w.kill_9();

    let (out, err, code) = w.run(&["list-sessions"]);
    assert_eq!((out.as_str(), code), ("", 1), "{err}");
    assert!(err.starts_with("no server running on "), "{err}");

    w.ok(&["start-server"]);
    w.repid();
    assert_eq!(structure(&w), before);
    let dead = w.ok(&[
        "list-panes",
        "-a",
        "-F",
        "#{pane_id} #{pane_dead}[#{pane_dead_status}#{pane_dead_signal}] #{?pane_pid,pid,}",
    ]);
    assert_eq!(dead, "%0 1[] \n%1 1[] \n%2 1[] \n%3 1[] \n%4 1[] \n");
    assert!(
        w.ok(&["capture-pane", "-p", "-t", "a:w1.0"])
            .contains("Pane is dead"),
    );
    assert!(
        w.ok(&["show-messages"])
            .contains("vtmux-persist: restored 2 sessions")
    );
    // New ids go on from the restored ones.
    w.ok(&["new-window", "-d", "-t", "b:1", "cat", "-"]);
    assert_eq!(
        w.ok(&["display", "-p", "-t", "b:1", "#{window_id} #{pane_id}"]),
        "@3 %5\n"
    );
}

/// `attach` with no server and a checkpoint there starts one, restores
/// it and attaches (decision 28): the client shows the restored session.
#[test]
fn attach_with_no_server_restores_and_attaches() {
    let mut w = World::start(&["cat", "-"], &[("VTMUX_PERSIST_INTERVAL_MS", "50")]);
    w.ok(&["set", "-s", "vtmux-persist", "layout"]);
    w.ok(&["set", "-g", "status-right", "pinned"]);
    w.ok(&["rename-window", "-t", "a:0", "restored"]);
    w.wait_saved();
    w.kill_9();
    let client = Attached::new(&w, &["attach", "-t", "a"]);
    wait_until("the attached client shows the restored pane", || {
        let screen = client.screen();
        screen.contains("Pane is dead") && screen.contains("0:restored*")
    });
    w.repid();
    assert_eq!(w.ok(&["list-clients", "-F", "#{client_session}"]), "a\n");
}

impl World {
    /// Type `word` into `pane` (`cat -`) and wait until the terminal echo and
    /// `cat`'s copy both show, so the next line's echo can't come between.
    fn echo(&self, pane: &str, word: &str) {
        self.ok(&["send-keys", "-t", pane, word, "Enter"]);
        wait_until("the pane echoing", || {
            self.ok(&["capture-pane", "-p", "-t", pane])
                .matches(word)
                .count()
                >= 2
        });
    }

    /// What `pane` shows (`capture-pane -p`, every row), its history size
    /// and cursor, and the whole of it with its history (`-S -`).
    fn shown(&self, pane: &str) -> (String, String, String) {
        (
            self.ok(&["capture-pane", "-p", "-t", pane]),
            self.ok(&["display", "-p", "-t", pane, "#{history_size}"]),
            self.ok(&["capture-pane", "-p", "-S", "-", "-t", pane]),
        )
    }
}

/// A `full` restore shows a pane's saved screen as it was, every row in
/// place, with the dead line on its blank last row; the restored server's
/// own checkpoint comes back the same, with no second dead line. (A
/// `cat -` pane with line1 to line3 typed came back with its first row
/// scrolled into history, and every restart scrolled it once more and
/// stacked another dead line.)
#[test]
fn a_full_restore_shows_the_saved_screen_and_one_dead_line() {
    let mut w = World::start(&["cat", "-"], &[("VTMUX_PERSIST_INTERVAL_MS", "50")]);
    w.ok(&["set", "-s", "vtmux-persist", "full"]);
    for word in ["line1", "line2", "line3"] {
        w.echo("a", word);
    }
    let (saved, _, _) = w.shown("a");
    let typed = "line1\nline1\nline2\nline2\nline3\nline3\n";
    assert!(saved.starts_with(typed), "{saved}");
    w.wait_saved();
    w.kill_9();

    w.ok(&["start-server"]);
    w.repid();
    let restored = w.shown("a");
    let (screen, history, all) = &restored;
    let rows: Vec<&str> = screen.lines().collect();
    assert_eq!(rows.len(), 24, "{screen}");
    let saved_rows: Vec<&str> = saved.lines().collect();
    assert_eq!(&rows[..23], &saved_rows[..23], "{screen}");
    assert!(rows[23].starts_with("Pane is dead (, "), "{screen}");
    assert_eq!(history, "0\n");
    assert_eq!(all, screen);

    // The restored server checkpoints the dead pane on its way out; the
    // next start shows it as it was, its dead line drawn once.
    w.kill_server();
    w.ok(&["start-server"]);
    w.repid();
    assert_eq!(w.shown("a"), restored);
}

/// `respawn-pane` (and `respawn-window`) start a restored pane's saved
/// command in its saved cwd.
#[test]
fn respawn_pane_restarts_the_saved_command_in_its_saved_cwd() {
    let mut w = World::start(&["cat", "-"], &[]);
    let cwd = std::fs::canonicalize(w.dir.path())
        .unwrap()
        .join("work dir");
    std::fs::create_dir(&cwd).unwrap();
    let cwd = cwd.to_str().unwrap().to_owned();
    w.ok(&["set", "-s", "vtmux-persist", "layout"]);
    w.ok(&[
        "new-window",
        "-d",
        "-t",
        "a:1",
        "-n",
        "w1",
        "-c",
        &cwd,
        "sh",
        "-c",
        "printf in=; pwd; exec cat -",
    ]);
    w.ok(&[
        "new-window",
        "-d",
        "-t",
        "a:2",
        "-n",
        "w2",
        "-c",
        &cwd,
        "cat",
        "-",
    ]);
    w.kill_server();
    w.ok(&["start-server"]);
    w.repid();
    assert_eq!(w.ok(&["display", "-p", "-t", "a:1", "#{pane_dead}"]), "1\n");
    w.ok(&["respawn-pane", "-t", "a:1"]);
    wait_until("the pane runs its command in its cwd", || {
        w.ok(&["capture-pane", "-p", "-t", "a:1"])
            .contains(&format!("in={cwd}\n"))
    });
    w.ok(&["respawn-window", "-t", "a:2"]);
    assert_eq!(
        w.ok(&[
            "list-panes",
            "-a",
            "-F",
            "#{window_name} #{pane_dead} #{pane_start_command} #{pane_start_path}"
        ]),
        format!(
            "w0 1 cat - {here}\nw1 0 sh -c \"printf in=; pwd; exec cat -\" {cwd}\nw2 0 cat - {cwd}\n",
            here = std::fs::canonicalize(w.dir.path()).unwrap().display()
        )
    );
    wait_until("the respawned pane runs cat", || {
        w.ok(&["display", "-p", "-t", "a:2", "#{pane_current_command}"]) == "cat\n"
    });
}

/// The config runs after the restore (decision 27): what it sets wins
/// over the checkpoint, runtime-only settings survive, and its
/// `new-session` of a restored name fails as tmux's duplicate does,
/// keeping the restored session.
#[test]
fn config_edits_since_the_save_win() {
    let mut w = World::start_with(&["cat", "-"], &[], None, Some("set -g @c old\n"));
    w.ok(&["set", "-s", "vtmux-persist", "layout"]);
    w.ok(&["set", "-g", "@runtime", "kept"]);
    w.ok(&["rename-window", "-t", "a:0", "saved"]);
    w.kill_server();
    std::fs::write(
        &w.config,
        "set -g @c new\nnew -d -s a -n fromconf cat -\nnew -d -s c -n cw cat -\nset -g @after yes\n",
    )
    .unwrap();
    w.ok(&["start-server"]);
    w.repid();
    assert_eq!(w.ok(&["show", "-gv", "@c"]), "new\n");
    assert_eq!(w.ok(&["show", "-gv", "@runtime"]), "kept\n");
    assert_eq!(w.ok(&["show", "-gv", "@after"]), "yes\n");
    assert_eq!(
        w.ok(&["list-windows", "-a", "-F", "#{session_name} #{window_name}"]),
        "a saved\nc cw\n"
    );
    // cfg.c `cfg_add_cause`: no client is attached, so `cfg_show_causes`
    // keeps them for the next source-file.
    let (out, err, _) = w.run(&["source-file", "/dev/null"]);
    assert_eq!(
        out + &err,
        format!("{}:2: duplicate session: a\n", w.config.display())
    );
}

/// A config runs on a restored server as on a fresh one: the first time
/// it sets an option or hook, that starts from its default, so what it
/// appends (`-a`) goes in once, not once more on every restart, and `-o`
/// does not find its own earlier value. Options the config does not set
/// keep their saved, runtime values. (Each restart added another
/// `terminal-features` item, another hook command and another `X`.)
#[test]
fn a_restored_config_appends_once_not_once_per_restart() {
    let conf = "set -s vtmux-persist layout\n\
                set -as terminal-features ',rv-term:RGB'\n\
                set-hook -ga after-new-window 'set -g @n 1'\n\
                set -ga status-right X\n\
                set -go @once yes\n";
    let mut w = World::start_with(&["cat", "-"], &[], None, Some(conf));
    w.ok(&["set", "-g", "status-left", "runtime"]);
    let observe = |w: &World| {
        [
            &["show", "-s", "terminal-features"][..],
            &["show-hooks", "-g", "after-new-window"],
            &["show", "-gv", "status-right"],
            &["show", "-gv", "@once"],
            &["show", "-gv", "status-left"],
            // The config's errors (none): `cfg_show_causes` keeps them for
            // the next source-file while no client is attached.
            &["source-file", "/dev/null"],
        ]
        .iter()
        .map(|args| {
            let (out, err, _) = w.run(args);
            out + &err
        })
        .collect::<Vec<_>>()
    };
    let fresh = observe(&w);
    assert!(fresh[0].contains("rv-term:RGB"), "{fresh:?}");
    for _ in 0..2 {
        w.kill_server();
        w.ok(&["start-server"]);
        w.repid();
        assert!(w.ok(&["show-messages"]).contains("restored 1 sessions"));
        assert_eq!(observe(&w), fresh);
    }
}

/// A config that turns `vtmux-persist` off gets no restore (spec,
/// *Non-goals*); the files stay.
#[test]
fn a_config_with_persist_off_restores_nothing() {
    let w = World::start_with(&["cat", "-"], &[], None, Some(""));
    w.ok(&["set", "-s", "vtmux-persist", "layout"]);
    w.kill_server();
    let manifest = std::fs::read(w.state().join(persist::MANIFEST)).unwrap();
    std::fs::write(&w.config, "set -s vtmux-persist off\n").unwrap();
    w.ok(&["new", "-d", "-s", "z", "cat", "-"]);
    assert_eq!(w.ok(&["list-sessions", "-F", "#{session_name}"]), "z\n");
    w.kill_server();
    assert_eq!(
        std::fs::read(w.state().join(persist::MANIFEST)).unwrap(),
        manifest
    );
}

/// `vtmux-persist-drop` (decision 30): the user options matching its
/// globs are not restored. A session's own value applies to it; the
/// others go by the global one, the config's when it sets it (here only
/// after the save), so `@murmur_*` goes from session `a` (and its window
/// and pane) and `@tool_*` from session `b`.
#[test]
fn vtmux_persist_drop_drops_those_user_options() {
    let mut w = World::start_with(&["cat", "-"], &[], None, Some(""));
    w.ok(&["set", "-s", "vtmux-persist", "layout"]);
    w.ok(&["new", "-d", "-s", "b", "cat", "-"]);
    w.ok(&["set", "-t", "a", "vtmux-persist-drop", "@murmur_*"]);
    for s in ["a", "b"] {
        w.ok(&["set", "-t", s, "@murmur_session_state", "working"]);
        w.ok(&["set", "-t", s, "@tool_x", "1"]);
        w.ok(&["set", "-t", s, "@keep", "yes"]);
        w.ok(&["set", "-w", "-t", s, "@murmur_w", "2"]);
        w.ok(&["set", "-p", "-t", s, "@murmur_p", "3"]);
    }
    w.kill_server();
    std::fs::write(&w.config, "set -g vtmux-persist-drop '@tool_*'\n").unwrap();
    w.ok(&["start-server"]);
    w.repid();
    let show = |args: &[&str]| w.run(args).0;
    for (s, murmur, tool) in [("a", "", "1\n"), ("b", "working\n", "")] {
        assert_eq!(
            show(&["show", "-v", "-t", s, "@murmur_session_state"]),
            murmur,
            "{s}"
        );
        assert_eq!(show(&["show", "-v", "-t", s, "@tool_x"]), tool, "{s}");
        assert_eq!(show(&["show", "-v", "-t", s, "@keep"]), "yes\n", "{s}");
        let murmur_wp = if murmur.is_empty() {
            ["", ""]
        } else {
            ["2\n", "3\n"]
        };
        assert_eq!(
            show(&["show", "-wv", "-t", s, "@murmur_w"]),
            murmur_wp[0],
            "{s}"
        );
        assert_eq!(
            show(&["show", "-pv", "-t", s, "@murmur_p"]),
            murmur_wp[1],
            "{s}"
        );
    }
}

/// A manifest that does not decode starts a fresh server (the command's
/// own session only), with why in `show-messages`; the files are kept and
/// not written over, not even by the shutdown checkpoint.
#[test]
fn a_corrupted_manifest_starts_a_fresh_server_and_keeps_the_files() {
    let mut w = World::start(&["cat", "-"], &[]);
    w.ok(&["set", "-s", "vtmux-persist", "layout"]);
    w.kill_server();
    let path = w.state().join(persist::MANIFEST);
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();
    w.ok(&["new", "-d", "-s", "z", "cat", "-"]);
    w.repid();
    assert_eq!(w.ok(&["list-sessions", "-F", "#{session_name}"]), "z\n");
    let log = w.ok(&["show-messages"]);
    assert!(
        log.contains("not restored: manifest: snapshot is corrupt")
            && log.contains("the files are kept"),
        "{log}"
    );
    w.ok(&["rename-session", "-t", "z", "changed"]);
    w.kill_server();
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert!(!w.state().join(persist::MANIFEST_PREV).exists());
}

/// A copy mode over more history than a snapshot page holds, which every
/// vtmux before 8c94c6e checkpointed in one page (the test hook stands in
/// for such a writer), is left out of the restore, as a live upgrade's last
/// resort leaves it: the session and the pane's own screen and history
/// come back, and the log says what was left out. (The whole checkpoint
/// was refused: `history chunk ... missing or corrupt`.)
#[test]
fn a_copy_mode_page_past_the_limit_is_left_out_of_the_restore() {
    let mut w = World::start_with(
        &["sh", "-c", "seq 70000; exec cat -"],
        &[("VTMUX_TEST_COPY_ONE_PAGE", "1")],
        None,
        Some("set -g history-limit 100000\n"),
    );
    let shown = |w: &World| {
        w.ok(&[
            "display",
            "-p",
            "-t",
            "a",
            "#{history_size} #{pane_in_mode}",
        ])
    };
    wait_until("the long history", || {
        w.ok(&[
            "display",
            "-p",
            "-t",
            "a",
            "#{history_size} #{pane_current_command}",
        ]) == "69977 cat\n"
    });
    let saved = w.ok(&["capture-pane", "-p", "-S", "-", "-t", "a"]);
    w.ok(&["set", "-s", "vtmux-persist", "full"]);
    w.ok(&["copy-mode", "-t", "a"]);
    assert_eq!(shown(&w), "69977 1\n");
    w.kill_server();

    // (A server with nothing restored and no session exits at once.)
    w.ok(&["new", "-d", "-s", "z", "cat", "-"]);
    w.repid();
    let log = w.ok(&["show-messages"]);
    assert_eq!(
        w.ok(&["list-sessions", "-F", "#{session_name}"]),
        "a\nz\n",
        "{log}"
    );
    assert_eq!(shown(&w), "69977 0\n");
    let restored = w.ok(&["capture-pane", "-p", "-S", "-", "-t", "a"]);
    assert!(
        restored.starts_with(saved.trim_end()),
        "{}",
        &restored[restored.len().saturating_sub(200)..]
    );
    assert!(
        log.contains("vtmux-persist: restored 1 sessions from ")
            && log.contains(
                ", without %0's copy mode: malformed snapshot: history page sizes don't add up"
            ),
        "{log}"
    );
}

/// The checkpoint is checked in a child process, by the dry run a live
/// upgrade uses, before it is imported: a rebuild that fails there (the
/// test hook makes it) leaves a fresh server, with the child's message.
#[test]
fn a_checkpoint_that_fails_the_check_in_the_child_is_not_imported() {
    let mut w = World::start(&["cat", "-"], &[]);
    w.ok(&["set", "-s", "vtmux-persist", "layout"]);
    w.kill_server();
    w.env.push(("VTMUX_TEST_RESUME_FAIL", "check".to_owned()));
    w.ok(&["new", "-d", "-s", "z", "cat", "-"]);
    w.repid();
    assert_eq!(w.ok(&["list-sessions", "-F", "#{session_name}"]), "z\n");
    let log = w.ok(&["show-messages"]);
    assert!(
        log.contains("not restored: manifest: test hook: the rebuild fails"),
        "{log}"
    );
}

/// Every file under `dir` and its bytes.
fn contents(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out: Vec<_> = files(dir)
        .into_iter()
        .map(|p| {
            let bytes = std::fs::read(&p).unwrap();
            (p, bytes)
        })
        .collect();
    out.sort();
    out
}

/// A checkpoint made into one a newer binary wrote.
type Newer = fn(&[u8]) -> Vec<u8>;

/// The checkpoint `bytes` as the next format version would mark it.
fn newer_format(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    let v = snap::version(bytes).unwrap() + 1;
    out[8..12].copy_from_slice(&v.to_le_bytes());
    out
}

/// The checkpoint `bytes` requiring a feature no build knows yet.
fn newer_feature(bytes: &[u8]) -> Vec<u8> {
    let mut s = snap::decode(bytes).unwrap();
    s.meta.requires.push("from-the-future".to_owned());
    snap::encode_manifest(&s)
}

/// A checkpoint directory holding a manifest (`newer` names which) that
/// a newer binary wrote, a newer format or a feature this one lacks, is
/// left alone (decision 6): no restore, not even from the other manifest,
/// and nothing written there for the server's lifetime, periodic or at
/// shutdown, with why in `show-messages`. The reviewer's repro: the
/// format-2 `manifest` was refused, `manifest.prev` restored, and the
/// next checkpoint rotated the format-2 file away.
#[test]
fn a_checkpoint_of_a_newer_binary_is_left_alone() {
    let cases: [(&str, Newer, &str); 3] = [
        (persist::MANIFEST, newer_format, "newer than this binary"),
        (
            persist::MANIFEST_PREV,
            newer_format,
            "newer than this binary",
        ),
        (
            persist::MANIFEST,
            newer_feature,
            "`from-the-future`, which this binary lacks",
        ),
    ];
    for (newer, make, why) in cases {
        let mut w = World::start(&["cat", "-"], &[("VTMUX_PERSIST_INTERVAL_MS", "50")]);
        w.ok(&["set", "-s", "vtmux-persist", "layout"]);
        w.kill_server();
        // A second checkpoint, from the restored server: `manifest` and
        // `manifest.prev` both there.
        w.ok(&["start-server"]);
        w.repid();
        w.kill_server();
        let dir = w.state();
        assert!(dir.join(persist::MANIFEST_PREV).is_file());
        let path = dir.join(newer);
        std::fs::write(&path, make(&std::fs::read(&path).unwrap())).unwrap();
        let before = contents(&dir);

        w.ok(&["new", "-d", "-s", "z", "cat", "-"]);
        w.repid();
        assert_eq!(
            w.ok(&["list-sessions", "-F", "#{session_name}"]),
            "z\n",
            "{newer}"
        );
        let log = w.ok(&["show-messages"]);
        assert!(
            log.contains(&format!("not restored: {newer}: "))
                && log.contains(why)
                && log.contains("left as it is"),
            "{newer}: {log}"
        );
        // Changes, then the shutdown checkpoint: nothing is written (by
        // it, nor by a periodic one, due every 50 ms meanwhile).
        w.ok(&["set", "-g", "@change", "1"]);
        w.ok(&["new-window", "-d", "-n", "later", "cat", "-"]);
        w.kill_server();
        assert_eq!(contents(&dir), before, "{newer}");
    }
}

/// A `manifest` corrupt in a format this binary reads falls back to
/// `manifest.prev`, and the restored server checkpoints as usual (unlike
/// a newer binary's checkpoint, above). Its panes' status is unknown:
/// tmux's default dead line has nothing before its comma, and the
/// format the spec suggests for that reads `restored`.
#[test]
fn a_corrupted_manifest_falls_back_to_the_previous_one() {
    let mut w = World::start(&["cat", "-"], &[]);
    w.ok(&["set", "-s", "vtmux-persist", "layout"]);
    w.kill_server();
    w.ok(&["start-server"]);
    w.repid();
    w.ok(&["rename-session", "-t", "a", "b"]);
    w.kill_server();
    let dir = w.state();
    let path = dir.join(persist::MANIFEST);
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();
    w.ok(&["start-server"]);
    w.repid();
    assert_eq!(w.ok(&["list-sessions", "-F", "#{session_name}"]), "a\n");
    let log = w.ok(&["show-messages"]);
    assert!(
        log.contains("restored 1 sessions from") && log.contains("manifest.prev"),
        "{log}"
    );
    let line = w.ok(&[
        "display",
        "-p",
        "-t",
        "a",
        "Pane is dead (#{?#{!=:#{pane_dead_status},},status #{pane_dead_status},\
         #{?#{!=:#{pane_dead_signal},},signal #{pane_dead_signal},restored}}, \
         #{t:pane_dead_time})",
    ]);
    assert!(line.starts_with("Pane is dead (restored, "), "{line}");
    w.kill_server();
    let (s, name) = persist::load(&dir).unwrap();
    assert_eq!((session_names(&s), name), (vec!["a"], persist::MANIFEST));
}

/// A config with `vtmux-persist off` restores nothing, and a newer
/// binary's checkpoint stays untouched even when `vtmux-persist` is
/// turned on at runtime afterwards.
#[test]
fn a_newer_checkpoint_stays_untouched_when_persist_is_turned_on_later() {
    let mut w = World::start_with(&["cat", "-"], &[], None, Some("set -s vtmux-persist off\n"));
    w.ok(&["set", "-s", "vtmux-persist", "layout"]);
    w.kill_server();
    let path = w.state().join(persist::MANIFEST);
    std::fs::write(&path, newer_format(&std::fs::read(&path).unwrap())).unwrap();
    let before = contents(&w.state());
    w.ok(&["new", "-d", "-s", "z", "cat", "-"]);
    w.repid();
    w.ok(&["set", "-s", "vtmux-persist", "layout"]);
    w.kill_server();
    assert_eq!(contents(&w.state()), before);
}

/// The image a live upgrade starts carries no persistence state: it too
/// leaves a newer binary's checkpoint alone, checking before it first
/// writes there.
#[test]
fn a_newer_checkpoint_stays_untouched_across_a_live_upgrade() {
    let mut w = World::start(&["cat", "-"], &[]);
    w.ok(&["set", "-s", "vtmux-persist", "layout"]);
    w.kill_server();
    let path = w.state().join(persist::MANIFEST);
    std::fs::write(&path, newer_format(&std::fs::read(&path).unwrap())).unwrap();
    let before = contents(&w.state());
    w.ok(&["new", "-d", "-s", "z", "cat", "-"]);
    w.repid();
    // The frozen handshake (`tests/upgrade.rs`): the image that ends up
    // serving answers the requester.
    let socket = w.ok(&["display", "-p", "#{socket_path}"]);
    let mut stream = std::os::unix::net::UnixStream::connect(socket.trim_end()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let request = protocol::ClientMsg::UpgradeRequest {
        binary: PathBuf::from(env!("CARGO_BIN_EXE_vtmux")),
    };
    protocol::write_msg(&mut stream, &request).unwrap();
    let answer = protocol::read_msg::<protocol::ServerMsg>(&mut stream).unwrap();
    assert!(
        matches!(
            answer,
            Some(protocol::ServerMsg::UpgradeResult { ok: true, .. })
        ),
        "{answer:?}"
    );
    w.ok(&["set", "-s", "vtmux-persist", "layout"]);
    w.ok(&["rename-session", "-t", "z", "changed"]);
    w.kill_server();
    assert_eq!(contents(&w.state()), before);
}
