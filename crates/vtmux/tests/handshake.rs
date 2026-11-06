//! The frozen frames end to end (vtmux-core `protocol` module docs, *Frozen
//! frames*): raw frames on a real `target/debug/vtmux` server's socket, the
//! way a binary of another protocol version sends them. vtmux only: tmux
//! has no such frames (live-upgrade spec, *Handshake*).
//!
//! Isolation: environment cleared, a private `TMUX_TMPDIR` and `HOME`, an
//! explicit `-L`, `-f /dev/null`, pane command pinned to `cat -`.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use vtmux_core::protocol::{
    self, ClientFlags, ClientMsg, ControlMode, Hello, PROTOCOL_VERSION, ServerMsg,
};
use vtmux_core::socket::Invocation;

struct Server {
    dir: tempfile::TempDir,
    label: String,
}

impl Server {
    /// A server with one detached session `a`.
    fn start() -> Self {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let server = Self {
            dir: tempfile::tempdir().expect("tempdir"),
            label: format!(
                "cli{}hs{}",
                std::process::id(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
        };
        let (_, err, code) = server.run(&["new", "-d", "-s", "a", "cat", "-"]);
        assert_eq!(code, 0, "new-session: {err}");
        server
    }

    fn run(&self, args: &[&str]) -> (String, String, i32) {
        let out = Command::new(env!("CARGO_BIN_EXE_vtmux"))
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

    /// A raw connection to the server's socket. Reads time out rather than
    /// hang a test whose frame never comes.
    fn connect(&self) -> UnixStream {
        let (path, _, _) = self.run(&["display", "-p", "#{socket_path}"]);
        let stream = UnixStream::connect(path.trim_end()).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
    }

    fn hello(&self, version: u32, control: ControlMode, argv: &[&str]) -> ClientMsg {
        ClientMsg::Hello(Hello {
            version,
            invocation: Invocation::Vtmux,
            pid: std::process::id(),
            argv: argv.iter().map(|w| w.as_bytes().to_vec()).collect(),
            cwd: self.dir.path().to_path_buf(),
            env: vec![("TERM".into(), "xterm-256color".into())],
            tty: None,
            flags: ClientFlags {
                control,
                ..ClientFlags::default()
            },
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.run(&["kill-server"]);
    }
}

fn read(stream: &mut UnixStream) -> Option<ServerMsg> {
    protocol::read_msg::<ServerMsg>(stream).expect("a whole frame in time")
}

#[test]
fn an_upgrade_request_is_answered_without_a_hello() {
    let server = Server::start();
    let mut stream = server.connect();
    protocol::write_msg(
        &mut stream,
        &ClientMsg::UpgradeRequest {
            binary: PathBuf::from(env!("CARGO_BIN_EXE_vtmux")),
        },
    )
    .unwrap();
    assert_eq!(
        read(&mut stream),
        Some(ServerMsg::UpgradeResult {
            ok: false,
            message: "not implemented".into(),
        })
    );
    assert_eq!(read(&mut stream), None, "the result ends the connection");
    assert_eq!(server.run(&["has", "-t", "a"]).2, 0, "the server runs on");
}

#[test]
fn a_command_client_of_another_version_is_told_to_upgrade() {
    let server = Server::start();
    let mut stream = server.connect();
    let version = PROTOCOL_VERSION + 1;
    protocol::write_msg(
        &mut stream,
        &server.hello(version, ControlMode::None, &["ls"]),
    )
    .unwrap();
    assert_eq!(
        read(&mut stream),
        Some(ServerMsg::Stderr(
            format!(
                "protocol version mismatch (client {version}, server {PROTOCOL_VERSION})\n\
                 run 'tmux vtmux-upgrade' to move the server onto this binary\n"
            )
            .into_bytes()
        ))
    );
    assert_eq!(
        read(&mut stream),
        Some(ServerMsg::Exit {
            code: 1,
            message: None,
            hup: false,
        })
    );
}

/// A control client sets `wait-exit` (which the server answers with the
/// unfrozen `Flags`), runs a command and leaves. Returns every frame it
/// was sent.
fn control_session(server: &Server, version: u32) -> Vec<ServerMsg> {
    let mut stream = server.connect();
    let hello = server.hello(version, ControlMode::Control, &["attach", "-t", "a"]);
    protocol::write_msg(&mut stream, &hello).unwrap();
    protocol::write_msg(
        &mut stream,
        &ClientMsg::Stdin(b"refresh-client -f wait-exit\ndisplay -p hello\n".to_vec()),
    )
    .unwrap();
    let mut frames = Vec::new();
    let mut stdout = Vec::new();
    while !stdout.windows(11).any(|w| w == b"hello\n%end ") {
        let msg = read(&mut stream).expect("the server stays");
        if let ServerMsg::Stdout(b) = &msg {
            stdout.extend_from_slice(b);
        }
        assert!(!matches!(msg, ServerMsg::Exit { .. }), "{frames:?}");
        frames.push(msg);
    }
    protocol::write_msg(&mut stream, &ClientMsg::StdinEof).unwrap();
    while let Some(msg) = read(&mut stream) {
        frames.push(msg);
    }
    assert!(
        matches!(frames.last(), Some(ServerMsg::Exit { code: 0, .. })),
        "{frames:?}"
    );
    frames
}

#[test]
fn a_control_client_of_another_version_is_served_frozen_frames_only() {
    let server = Server::start();
    // This build's control client is sent `Flags`: the filter below is
    // what keeps it from another version's.
    let same = control_session(&server, PROTOCOL_VERSION);
    assert!(
        same.iter().any(|m| matches!(m, ServerMsg::Flags { .. })),
        "{same:?}"
    );
    for version in [PROTOCOL_VERSION + 1, u32::MAX] {
        let other = control_session(&server, version);
        assert!(other.iter().all(ServerMsg::is_frozen), "{other:?}");
    }
}
