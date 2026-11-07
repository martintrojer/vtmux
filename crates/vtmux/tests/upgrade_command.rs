//! `vtmux-upgrade` from the command line against a real
//! `target/debug/vtmux` server (live-upgrade spec, *Command*, *Sequence*
//! steps 1-4; the exec itself is tests/upgrade.rs'), and the client's fallback to the
//! frozen `UpgradeRequest` against a server of another version (a fake
//! one here). vtmux only: tmux has no such command.
//!
//! Isolation: environment cleared, a private `TMUX_TMPDIR` and `HOME`, an
//! explicit `-L`, `-f /dev/null`, pane commands pinned to `cat -`. Every
//! server is killed when its test ends.

use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use vtmux_core::protocol::{self, ClientMsg, ServerMsg};

const VTMUX: &str = env!("CARGO_BIN_EXE_vtmux");

struct Server {
    dir: tempfile::TempDir,
    label: String,
}

impl Server {
    /// A server with one detached session `a`, started as `start`.
    fn start_as(dir: tempfile::TempDir, start: &Path) -> Self {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let server = Self {
            dir,
            label: format!(
                "cli{}up{}",
                std::process::id(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
        };
        let (_, err, code) = server.run_as(start, &["new", "-d", "-s", "a", "cat", "-"]);
        assert_eq!(code, 0, "new-session: {err}");
        server
    }

    fn start() -> Self {
        Self::start_as(tempfile::tempdir().expect("tempdir"), Path::new(VTMUX))
    }

    fn run(&self, args: &[&str]) -> (String, String, i32) {
        self.run_as(Path::new(VTMUX), args)
    }

    fn run_as(&self, binary: &Path, args: &[&str]) -> (String, String, i32) {
        let out = Command::new(binary)
            .args(["-L", &self.label, "-f", "/dev/null"])
            .args(args)
            .current_dir(self.dir.path())
            .env_clear()
            .env("HOME", self.dir.path())
            .env("TMUX_TMPDIR", self.dir.path())
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color")
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .output()
            .expect("run vtmux");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.code().unwrap_or(-1),
        )
    }

    /// An executable `sh` script `name` in the server's directory.
    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.run(&["kill-server"]);
    }
}

/// Step 1 and 4 refusals reach the command line with exit 1, and the
/// server runs on.
#[test]
fn a_refused_upgrade_says_why_and_the_server_runs_on() {
    let server = Server::start();
    let garbage = server.dir.path().join("garbage");
    std::fs::write(&garbage, b"\x7fnot a program").unwrap();
    std::fs::set_permissions(&garbage, std::fs::Permissions::from_mode(0o755)).unwrap();
    let failing = server.script("failing", "echo 'snapshot: not mine' >&2; exit 2");
    for (args, want) in [
        (
            vec!["vtmux-upgrade", "-b", "missing"],
            format!(
                "{}: No such file or directory\n",
                // The client's directory, as getcwd(3) reports it.
                std::fs::canonicalize(server.dir.path())
                    .unwrap()
                    .join("missing")
                    .display()
            ),
        ),
        (
            vec!["vtmux-upgrade", "-b", garbage.to_str().unwrap()],
            format!("{}: Exec format error\n", garbage.display()),
        ),
        (
            vec!["vtmux-upgrade", "-b", failing.to_str().unwrap()],
            "snapshot: not mine\n".to_owned(),
        ),
        (
            vec!["vtmux-upgrade", "x"],
            "command vtmux-upgrade: too many arguments (need at most 0)\n".to_owned(),
        ),
    ] {
        assert_eq!(server.run(&args), (String::new(), want, 1), "{args:?}");
    }
    assert_eq!(server.run(&["has", "-t", "a"]).2, 0);
    assert_eq!(
        server.run(&["lscm", "vtmux-upgrade"]).0,
        "vtmux-upgrade [-b binary]\n"
    );
}

/// Decision 10: a `run-shell` job still running after the wait refuses
/// the upgrade, naming it. The job waits on this server (through the
/// shim), so it ends with it.
#[test]
fn a_running_job_refuses_the_upgrade() {
    let server = Server::start();
    let pass = server.script("pass", "cat > /dev/null");
    let job = "tmux wait-for never";
    assert_eq!(server.run(&["run-shell", "-b", job]).2, 0);
    let (out, err, code) = server.run(&["vtmux-upgrade", "-b", pass.to_str().unwrap()]);
    assert_eq!((out.as_str(), code), ("", 1));
    let want = format!("still running after 3 s: job '{job}'");
    assert!(err.starts_with(&want) && err.ends_with('\n'), "{err}");
}

/// Decision 22: without `-b` the server moves onto the path it was
/// started as, resolved at request time: after a relink of that path the
/// new binary runs the dry run.
#[test]
fn without_b_the_relinked_start_path_is_the_binary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let link = bin.join("vtmux");
    std::os::unix::fs::symlink(VTMUX, &link).unwrap();
    let server = Server::start_as(dir, &link);
    let new = server.script("new", "echo \"relinked $1\" >&2; exit 1");
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&new, &link).unwrap();
    let (_, err, code) = server.run(&["vtmux-upgrade"]);
    assert_eq!(
        (err.as_str(), code),
        ("relinked --vtmux-check-snapshot\n", 1)
    );
}

/// A server of another version, faked: it answers a `Hello` with the
/// version mismatch, then reads one frame on the next connection and
/// answers it with `answer` (or closes). Returns that frame.
fn fake_server(socket: &Path, answer: Option<ServerMsg>) -> std::thread::JoinHandle<ClientMsg> {
    let listener = UnixListener::bind(socket).unwrap();
    std::thread::spawn(move || {
        let accept = || {
            let (s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            s
        };
        let mut first = accept();
        let hello = protocol::read_msg::<ClientMsg>(&mut first)
            .unwrap()
            .unwrap();
        assert!(matches!(hello, ClientMsg::Hello(_)), "{hello:?}");
        let mismatch = b"protocol version mismatch (client 9, server 8)\n".to_vec();
        protocol::write_msg(&mut first, &ServerMsg::Stderr(mismatch)).unwrap();
        let exit = ServerMsg::Exit {
            code: 1,
            message: None,
            hup: false,
        };
        protocol::write_msg(&mut first, &exit).unwrap();
        drop(first);
        let mut second = accept();
        let request = protocol::read_msg::<ClientMsg>(&mut second)
            .unwrap()
            .unwrap();
        if let Some(answer) = answer {
            protocol::write_msg(&mut second, &answer).unwrap();
        }
        request
    })
}

fn client(dir: &Path, socket: &Path, args: &[&str]) -> (String, i32) {
    let out = Command::new(VTMUX)
        .arg("-S")
        .arg(socket)
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("HOME", dir)
        .env("TERM", "xterm-256color")
        .output()
        .expect("run vtmux");
    (
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// *Command*: a server of another version refuses the `Hello`, so the
/// client asks again with the frozen `UpgradeRequest`: for `-b` from its
/// directory, else for its own binary (the mismatch hint's "this
/// binary"), and prints the result.
#[test]
fn a_client_of_another_version_asks_with_the_frozen_request() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("s");
    let ok = ServerMsg::UpgradeResult {
        ok: true,
        message: "client /dev/pts/9 can't re-attach by itself and will be detached".into(),
    };
    let server = fake_server(&socket, Some(ok));
    let (err, code) = client(dir.path(), &socket, &["vtmux-upgrade"]);
    assert_eq!(
        (err.as_str(), code),
        (
            "client /dev/pts/9 can't re-attach by itself and will be detached\n",
            0
        )
    );
    let own = std::fs::canonicalize(VTMUX).unwrap();
    assert_eq!(
        server.join().unwrap(),
        ClientMsg::UpgradeRequest { binary: own }
    );

    std::fs::remove_file(&socket).unwrap();
    let refused = ServerMsg::UpgradeResult {
        ok: false,
        message: "snapshot: newer".into(),
    };
    let server = fake_server(&socket, Some(refused));
    let (err, code) = client(dir.path(), &socket, &["vtmux-upgrade", "-b", "new/vtmux"]);
    assert_eq!((err.as_str(), code), ("snapshot: newer\n", 1));
    let binary = std::fs::canonicalize(dir.path()).unwrap().join("new/vtmux");
    assert_eq!(server.join().unwrap(), ClientMsg::UpgradeRequest { binary });

    // A server from before the handshake drops the request: the mismatch
    // is all the client can say.
    std::fs::remove_file(&socket).unwrap();
    let server = fake_server(&socket, None);
    let (err, code) = client(dir.path(), &socket, &["vtmux-upgrade"]);
    assert_eq!(
        (err.as_str(), code),
        ("protocol version mismatch (client 9, server 8)\n", 1)
    );
    server.join().unwrap();
}

/// The dry run passes but the `execve` after it fails (the binary removed
/// itself meanwhile): the requester hears the failure, never a success,
/// and the server runs on as it was.
#[test]
fn an_exec_that_fails_after_the_dry_run_is_reported_as_failed() {
    let server = Server::start();
    let gone = server.script("gone", "rm -f \"$0\"\nexit 0");
    let (out, err, code) = server.run(&["vtmux-upgrade", "-b", gone.to_str().unwrap()]);
    assert_eq!(
        (out.as_str(), err.as_str(), code),
        (
            "",
            format!("{}: No such file or directory\n", gone.display()).as_str(),
            1
        )
    );
    assert!(!gone.exists());
    assert_eq!(server.run(&["has", "-t", "a"]).2, 0);
}
