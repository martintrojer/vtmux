//! The frozen frames end to end (vtmux-core `protocol` module docs, *Frozen
//! frames*): raw frames on a real `target/debug/vtmux` server's socket, the
//! way a binary of another protocol version sends them. vtmux only: tmux
//! has no such frames (live-upgrade spec, *Handshake*).
//!
//! Isolation: environment cleared, a private `TMUX_TMPDIR` and `HOME`, an
//! explicit `-L`, `-f /dev/null`, pane command pinned to `cat -`.

mod socket_env;

#[test]
fn socket_paths_ignore_long_tmpdir() {
    socket_env::with_long_tmpdir("an_upgrade_request_is_answered_without_a_hello");
}

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
            dir: tempfile::tempdir_in("/tmp").expect("tempdir"),
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
            ext: vtmux_core::protocol::HelloExt::default(),
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
    // A binary that is not there: refused, so the server answers (one
    // that execs closes the connection instead, tests/upgrade.rs).
    let binary = server.dir.path().join("no-vtmux");
    protocol::write_msg(
        &mut stream,
        &ClientMsg::UpgradeRequest {
            binary: binary.clone(),
        },
    )
    .unwrap();
    assert_eq!(
        read(&mut stream),
        Some(ServerMsg::UpgradeResult {
            ok: false,
            message: format!("{}: No such file or directory", binary.display()),
        })
    );
    assert_eq!(read(&mut stream), None, "the result ends the connection");
    assert_eq!(server.run(&["has", "-t", "a"]).2, 0, "the server runs on");
}

/// The server's working directory is not the requester's: a relative
/// binary is refused (protocol module docs, *Frozen frames*).
#[test]
fn an_upgrade_request_with_a_relative_binary_is_refused() {
    let server = Server::start();
    let mut stream = server.connect();
    let request = ClientMsg::UpgradeRequest {
        binary: PathBuf::from("vtmux"),
    };
    protocol::write_msg(&mut stream, &request).unwrap();
    assert_eq!(
        read(&mut stream),
        Some(ServerMsg::UpgradeResult {
            ok: false,
            message: "binary must be an absolute path".into(),
        })
    );
}

/// `frame` (one encoded frame) with its payload changed by `f` and the
/// length fixed.
fn reframe(frame: &[u8], f: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut payload = frame[4..].to_vec();
    f(&mut payload);
    let mut out = u32::try_from(payload.len()).unwrap().to_le_bytes().to_vec();
    out.extend_from_slice(&payload);
    out
}

/// A Hello's version is read first (protocol module docs): a Hello of
/// another version that this server can't decode (bytes past its fields,
/// an invocation byte it doesn't know) still gets the mismatch, with the
/// upgrade hint for a newer client, not a dropped connection.
#[test]
fn a_hello_of_another_version_that_does_not_decode_gets_the_mismatch() {
    let server = Server::start();
    let hint = "run 'tmux vtmux-upgrade' to move the server onto this binary\n";
    let newer = PROTOCOL_VERSION + 1;
    let older = PROTOCOL_VERSION - 1;
    for (version, hint, how) in [
        (newer, hint, "trailing"),
        (newer, hint, "invocation"),
        (u32::MAX, hint, "trailing"),
        (older, "", "trailing"),
    ] {
        for control in [ControlMode::None, ControlMode::Control] {
            let hello = protocol::encode(&server.hello(version, control, &["ls"])).unwrap();
            let raw = reframe(&hello, |p| match how {
                "trailing" => p.push(b'x'),
                _ => p[5] = 2,
            });
            assert!(protocol::decode::<ClientMsg>(&raw).is_err());
            let mut stream = server.connect();
            std::io::Write::write_all(&mut stream, &raw).unwrap();
            let line = format!(
                "protocol version mismatch (client {version}, server {PROTOCOL_VERSION})\n"
            );
            assert_eq!(
                read(&mut stream),
                Some(ServerMsg::Stderr(format!("{line}{hint}").into_bytes())),
                "{version} {how} {control:?}"
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
    }
    assert_eq!(server.run(&["has", "-t", "a"]).2, 0, "the server runs on");
}

/// A later version's Hello fields go in the extension area after the
/// ones this build knows (none set here: `upgrade` 0, no `reattach`, no
/// `pixels`, no `control_chars`), and this server skips them: such a
/// control client is served, and so is a command client of this version.
#[test]
fn a_hello_with_a_later_extension_area_is_served() {
    let server = Server::start();
    let with_ext = |msg: &ClientMsg| {
        reframe(&protocol::encode(msg).unwrap(), |p| {
            p.truncate(p.len() - 4);
            p.extend_from_slice(&19u32.to_le_bytes());
            p.extend_from_slice(b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00later");
        })
    };
    let mut stream = server.connect();
    let hello = server.hello(
        PROTOCOL_VERSION,
        ControlMode::None,
        &["display", "-p", "hi"],
    );
    std::io::Write::write_all(&mut stream, &with_ext(&hello)).unwrap();
    assert_eq!(read(&mut stream), Some(ServerMsg::Stdout(b"hi\n".to_vec())));
    assert!(
        matches!(read(&mut stream), Some(ServerMsg::Exit { code: 0, .. })),
        "command client"
    );
    let mut stream = server.connect();
    let hello = server.hello(
        PROTOCOL_VERSION + 1,
        ControlMode::Control,
        &["display", "-p", "hi"],
    );
    // No StdinEof: a control client that is not attached leaves when its
    // command is done (tmux server-client.c `server_client_command_done`),
    // so the server may have closed the socket before a later write, which
    // then fails with EPIPE. The frames it sent are still read below.
    std::io::Write::write_all(&mut stream, &with_ext(&hello)).unwrap();
    let mut stdout = Vec::new();
    let mut last = None;
    while let Some(msg) = read(&mut stream) {
        if let ServerMsg::Stdout(b) = &msg {
            stdout.extend_from_slice(b);
        }
        last = Some(msg);
    }
    assert!(
        stdout.windows(8).any(|w| w == b"hi\n%end "),
        "{}",
        String::from_utf8_lossy(&stdout)
    );
    assert!(
        matches!(last, Some(ServerMsg::Exit { code: 0, .. })),
        "{last:?}"
    );
}

/// A control client skips a server frame it doesn't know (a later server's
/// new frozen frame) and goes on: here a fake server sends tag `0x82`,
/// then output and `Exit 0`, which the client prints and exits with.
#[test]
fn a_control_client_skips_a_frame_it_does_not_know() {
    let dir = tempfile::tempdir_in("/tmp").expect("tempdir");
    let path = dir.path().join("fake");
    let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
    let fake = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let hello = protocol::read_msg::<ClientMsg>(&mut stream).expect("a Hello");
        assert!(matches!(hello, Some(ClientMsg::Hello(_))), "{hello:?}");
        let mut out = vec![6, 0, 0, 0, 0x82, 1, 0, 0, 0, 0xff];
        out.extend(protocol::encode(&ServerMsg::Stdout(b"after\n".to_vec())).unwrap());
        out.extend(
            protocol::encode(&ServerMsg::Exit {
                code: 0,
                message: None,
                hup: false,
            })
            .unwrap(),
        );
        std::io::Write::write_all(&mut stream, &out).unwrap();
        // Wait for the client to leave.
        let mut rest = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stream, &mut rest);
    });
    let out = Command::new(env!("CARGO_BIN_EXE_vtmux"))
        .arg("-S")
        .arg(&path)
        .args(["-C", "attach"])
        .current_dir(dir.path())
        .env_clear()
        .env("HOME", dir.path())
        .env("TERM", "xterm-256color")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run vtmux");
    fake.join().unwrap();
    assert_eq!(
        (String::from_utf8_lossy(&out.stdout), out.status.code()),
        ("after\n%exit\n".into(), Some(0)),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A command client of another version is refused; a newer one is also
/// told how to move the server onto its binary, an older one not (that
/// would be a downgrade).
#[test]
fn a_command_client_of_another_version_is_refused_and_a_newer_one_told_to_upgrade() {
    let server = Server::start();
    let hint = "run 'tmux vtmux-upgrade' to move the server onto this binary\n";
    for (version, hint) in [(PROTOCOL_VERSION + 1, hint), (PROTOCOL_VERSION - 1, "")] {
        let mut stream = server.connect();
        let hello = server.hello(version, ControlMode::None, &["ls"]);
        protocol::write_msg(&mut stream, &hello).unwrap();
        let line =
            format!("protocol version mismatch (client {version}, server {PROTOCOL_VERSION})\n");
        assert_eq!(
            read(&mut stream),
            Some(ServerMsg::Stderr(format!("{line}{hint}").into_bytes()))
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
}

/// A control client sets `wait-exit` (which the server answers with
/// `Flags`), runs a command and leaves. Returns every frame it was sent.
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

/// A control client of another version is accepted and gets `Flags` and
/// `Exit 0`. That it gets only frozen frames is not checked here: the
/// server sends a control client of any version nothing else, so such a
/// check could not fail. vtmux-server's unit test
/// `a_frozen_client_is_sent_frozen_frames_only` covers the filter.
#[test]
fn a_control_client_of_another_version_is_accepted_with_flags_and_exit() {
    let server = Server::start();
    for version in [PROTOCOL_VERSION, PROTOCOL_VERSION + 1, u32::MAX] {
        let frames = control_session(&server, version);
        assert!(
            frames.contains(&ServerMsg::Flags { wait_exit: true }),
            "{version}: {frames:?}"
        );
    }
}

/// A control client of another version is served commands and frozen
/// frames, and `detach-client -E` leaves it running as it does any
/// control client (tmux's reads `MSG_EXEC` only once attached, which a
/// control client never is): no `Exec`, and the next command answers.
#[test]
fn a_control_client_of_another_version_outlives_detach_exec() {
    let server = Server::start();
    let mut stream = server.connect();
    let hello = server.hello(
        PROTOCOL_VERSION + 1,
        ControlMode::Control,
        &["attach", "-t", "a"],
    );
    protocol::write_msg(&mut stream, &hello).unwrap();
    let line = ClientMsg::Stdin(b"detach-client -E 'exit 3'\ndisplay -p still\n".to_vec());
    protocol::write_msg(&mut stream, &line).unwrap();
    let mut frames = Vec::new();
    while let Some(msg) = read(&mut stream) {
        let done = matches!(&msg, ServerMsg::Stdout(b) if b.windows(5).any(|w| w == b"still"));
        frames.push(msg);
        if done {
            break;
        }
    }
    assert!(
        !frames.iter().any(|m| matches!(m, ServerMsg::Exec { .. })),
        "{frames:?}"
    );
    assert!(
        matches!(frames.last(), Some(ServerMsg::Stdout(_))),
        "{frames:?}"
    );
}

/// An attach client of protocol 9 (frozen `Hello`, then its 8-byte
/// `Resize`, which this build's is not) is turned away with the mismatch,
/// not attached on a connection whose next frame never decodes.
#[test]
fn an_attach_client_of_protocol_9_gets_the_mismatch() {
    let server = Server::start();
    let mut stream = server.connect();
    let ClientMsg::Hello(mut hello) = server.hello(
        protocol::FROZEN_SINCE,
        ControlMode::None,
        &["attach", "-t", "a"],
    ) else {
        unreachable!()
    };
    hello.tty = Some(vtmux_core::protocol::TtyInfo {
        name: String::new(),
        term: "xterm-256color".into(),
        cols: 80,
        rows: 24,
        features: Vec::new(),
    });
    protocol::write_msg(&mut stream, &ClientMsg::Hello(hello)).unwrap();
    let line = format!(
        "protocol version mismatch (client {}, server {PROTOCOL_VERSION})\n",
        protocol::FROZEN_SINCE
    );
    assert_eq!(
        read(&mut stream),
        Some(ServerMsg::Stderr(line.into_bytes()))
    );
    assert_eq!(
        read(&mut stream),
        Some(ServerMsg::Exit {
            code: 1,
            message: None,
            hup: false,
        })
    );
    assert_eq!(server.run(&["lsc"]).0, "", "not attached");
}

/// A whole frame whose field runs past its end (protocol 9's `Resize`)
/// is never completed by more bytes: the client is sent `Exit 1` with
/// why, a terminal client after its teardown, and the server serves on.
#[test]
fn a_whole_frame_that_does_not_decode_ends_the_client_with_why() {
    let server = Server::start();
    let short_resize = [9, 0, 0, 0, 2, 80, 0, 0, 0, 24, 0, 0, 0];
    for terminal in [false, true] {
        let mut stream = server.connect();
        let control = if terminal {
            ControlMode::None
        } else {
            ControlMode::Control
        };
        let ClientMsg::Hello(mut hello) = server.hello(PROTOCOL_VERSION, control, &["attach"])
        else {
            unreachable!()
        };
        if terminal {
            hello.tty = Some(vtmux_core::protocol::TtyInfo {
                name: String::new(),
                term: "xterm-256color".into(),
                cols: 80,
                rows: 24,
                features: Vec::new(),
            });
        }
        protocol::write_msg(&mut stream, &ClientMsg::Hello(hello)).unwrap();
        if terminal {
            while read(&mut stream) != Some(ServerMsg::Ready) {}
        }
        wait_for_clients(&server, 1);
        std::io::Write::write_all(&mut stream, &short_resize).unwrap();
        let mut frames = Vec::new();
        while let Some(msg) = read(&mut stream) {
            frames.push(msg);
        }
        assert_eq!(
            frames.last(),
            Some(&ServerMsg::Exit {
                code: 1,
                message: Some("malformed frame: field cut short".into()),
                hup: false,
            }),
            "terminal {terminal}"
        );
        // A terminal client's terminal is given back first (`?1049l`).
        let teardown = frames
            .iter()
            .any(|m| matches!(m, ServerMsg::Frame(b) if b.windows(8).any(|w| w == b"\x1b[?1049l")));
        assert_eq!(teardown, terminal, "{frames:?}");
        wait_for_clients(&server, 0);
    }
    assert_eq!(server.run(&["has", "-t", "a"]).2, 0, "the server runs on");
}

/// Waits (at most 10 s) for `lsc` to list `n` clients.
fn wait_for_clients(server: &Server, n: usize) {
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while server.run(&["lsc"]).0.lines().count() != n {
        assert!(std::time::Instant::now() < end, "{n} clients");
        std::thread::sleep(Duration::from_millis(20));
    }
}
