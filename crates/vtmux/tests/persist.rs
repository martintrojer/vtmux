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
    label: String,
    /// Extra environment for every call (the server inherits the first's).
    env: Vec<(&'static str, String)>,
    pid: Option<rustix::process::Pid>,
}

impl World {
    /// A server with session `a` running `argv`, its checkpoints in
    /// `$XDG_STATE_HOME/vtmux/<label>`.
    fn start(argv: &[&str], env: &[(&'static str, &str)]) -> Self {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let mut w = Self {
            dir: tempfile::tempdir().expect("tempdir"),
            label: format!(
                "cli{}ps{}",
                std::process::id(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
            env: env.iter().map(|&(k, v)| (k, v.to_owned())).collect(),
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
            .args(["-L", &self.label, "-f", "/dev/null"])
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

    fn state(&self) -> PathBuf {
        self.dir
            .path()
            .join("state")
            .join("vtmux")
            .join(&self.label)
    }

    /// Wait for the server process to be gone.
    fn gone(&self) {
        let pid = self.pid.expect("server pid");
        wait_until("the server exits", || {
            rustix::process::test_kill_process(pid).is_err()
        });
    }

    fn kill_server(&self) {
        self.ok(&["kill-server"]);
        self.gone();
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = self.run(&["kill-server"]);
        common::reap(self.pid);
    }
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

/// [`SECRET`] as the snapshot row codec writes a run of plain cells: each
/// cell's codepoint shifted past its width bits, as a LEB128 varint
/// (`vtmux-snapshot` `row.rs`, `text_of`).
fn secret_encoded() -> Vec<u8> {
    let mut out = Vec::new();
    for c in SECRET.chars() {
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

/// `layout` writes no screen (decision 17): grep the directory for a
/// pane's text, shown on its screen and frozen in copy mode. `full` is
/// the control: the same text is found, so the search can find it.
#[test]
fn a_layout_checkpoint_on_disk_holds_no_pane_text() {
    for (setting, want) in [("layout", false), ("full", true)] {
        let w = World::start(&PRINTS_SECRET, &[]);
        w.ok(&["set", "-s", "vtmux-persist", setting]);
        wait_until("the pane shows the text", || {
            w.ok(&["capture-pane", "-p", "-t", "a"]).contains(SECRET)
        });
        w.ok(&["copy-mode", "-t", "a"]);
        w.kill_server();
        let dir = w.state();
        let (s, _) = persist::load(&dir).unwrap();
        assert_eq!(session_names(&s), ["a"], "{setting}");
        assert_eq!(shows_secret(&s), want, "{setting}");
        let encoded = secret_encoded();
        let on_disk = files(&dir).iter().any(|f| {
            let bytes = std::fs::read(f).unwrap();
            contains(&bytes, SECRET.as_bytes()) || contains(&bytes, &encoded)
        });
        assert_eq!(on_disk, want, "{setting}");
    }
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
    let manifest = w.state().join(persist::MANIFEST);
    wait_until("the first checkpoint", || manifest.is_file());
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
