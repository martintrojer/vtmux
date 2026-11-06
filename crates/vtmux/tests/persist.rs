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
