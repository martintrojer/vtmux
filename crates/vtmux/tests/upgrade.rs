//! Live upgrade end to end (live-upgrade spec, *Live upgrade*): a real
//! `target/debug/vtmux` server asked over the frozen `UpgradeRequest` to
//! move onto a binary execs it in place. vtmux only: tmux has no upgrade.
//!
//! Isolation: environment cleared, a private `TMUX_TMPDIR` under /tmp and
//! `HOME`, an explicit `-L`, `-f /dev/null`, pane commands pinned to
//! `cat -`, windows named, `status-right` pinned. Every wait is for state
//! (a listing, a line, bytes on a terminal), never a fixed sleep.
mod common;

use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use vtmux_core::protocol::{self, ClientMsg, ServerMsg};

const BIN: &str = env!("CARGO_BIN_EXE_vtmux");

struct World {
    label: String,
    home: tempfile::TempDir,
    sockets: tempfile::TempDir,
}

impl World {
    fn new() -> Self {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        Self {
            label: format!(
                "cli{}up{}",
                std::process::id(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
            home: tempfile::tempdir().unwrap(),
            // Under /tmp: the socket path must fit `sun_path`.
            sockets: tempfile::Builder::new()
                .prefix("vtu")
                .tempdir_in("/tmp")
                .unwrap(),
        }
    }

    fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("HOME", self.home.path().display().to_string()),
            ("TMUX_TMPDIR", self.sockets.path().display().to_string()),
            ("SHELL", "/bin/sh".into()),
            ("TERM", "xterm-256color".into()),
            ("LC_ALL", "C.UTF-8".into()),
            ("PATH", std::env::var("PATH").unwrap_or_default()),
        ]
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(["-L", &self.label, "-f", "/dev/null"])
            .args(args)
            .current_dir(self.home.path())
            .env_clear()
            .envs(self.env());
        cmd
    }

    /// stdout of a command that must succeed.
    fn run(&self, args: &[&str]) -> String {
        let out = self.command(args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Two sessions, three windows, four `cat -` panes, and state of every
    /// kind the listings show. `server_env` goes to the server's process
    /// environment (the test hooks).
    fn populate(&self, server_env: &[(&str, &str)]) {
        let out = self
            .command(&[
                "new", "-d", "-s", "a", "-n", "w1", "-x", "80", "-y", "24", "cat", "-",
            ])
            .envs(server_env.iter().copied())
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        for args in [
            &["set", "-g", "status-right", "R"][..],
            &["neww", "-d", "-t", "a", "-n", "w2", "cat", "-"],
            &["splitw", "-d", "-t", "a:w2", "cat", "-"],
            &["new", "-d", "-s", "b", "-n", "x", "cat", "-"],
            &["set", "-g", "@user", "hi"],
            &["set", "-w", "-t", "a:w2", "@wopt", "1"],
            &["set", "-p", "-t", "a:w2.1", "@popt", "2"],
            &["set", "-s", "escape-time", "7"],
            &["setenv", "-g", "UPG", "carried"],
            &["setenv", "-t", "b", "SESS", "env"],
            &["set-hook", "-g", "after-new-window", "set -g @hooked 1"],
            &["bind", "-T", "root", "F9", "display", "x"],
            &["setb", "-b", "named", "data"],
            &["select-pane", "-t", "a:w1", "-T", "t1"],
            &["select-pane", "-t", "a:w2.0", "-T", "t2"],
            &["select-pane", "-t", "a:w2.1", "-T", "t3"],
            &["select-pane", "-t", "b:x", "-T", "t4"],
            &["resize-pane", "-t", "a:w2.1", "-y", "5"],
        ] {
            self.run(args);
        }
    }

    fn pid(&self) -> String {
        self.run(&["display", "-p", "#{pid}"]).trim().to_owned()
    }

    /// Every listing an upgrade must leave as it was.
    fn observe(&self) -> String {
        let panes = "#{session_name}:#{window_index}.#{pane_index} #{pane_id} #{pane_pid} \
                     #{pane_tty} #{pane_width}x#{pane_height} #{pane_left},#{pane_top} \
                     #{pane_active} #{pane_title} #{pane_current_command} #{pane_dead} \
                     #{history_size} #{cursor_x},#{cursor_y}";
        let mut out = String::new();
        for args in [
            &[
                "display",
                "-p",
                "#{pid} #{start_time} #{socket_path} #{version}",
            ][..],
            &["ls"],
            &["lsw", "-a"],
            &[
                "lsw",
                "-a",
                "-F",
                "#{window_id} #{window_name} #{window_layout} #{window_flags}",
            ],
            &["lsp", "-a"],
            &["lsp", "-a", "-F", panes],
            &["lsc"],
            &["lsb"],
            &["lsk"],
            &["show", "-g"],
            &["show", "-gw"],
            &["show", "-s"],
            &["show", "-t", "a"],
            &["show", "-w", "-t", "a:w2"],
            &["show", "-p", "-t", "a:w2.1"],
            &["show", "-gH"],
            &["showenv", "-g"],
            &["showenv", "-t", "b"],
            &["show-hooks", "-g"],
        ] {
            writeln!(out, "$ {args:?}").unwrap();
            out.push_str(&self.run(args));
        }
        out
    }

    /// The pane pids, each still running.
    fn pane_pids(&self) -> Vec<String> {
        let pids: Vec<String> = self
            .run(&["lsp", "-a", "-F", "#{pane_pid}"])
            .lines()
            .map(str::to_owned)
            .collect();
        for pid in &pids {
            let raw = rustix::process::Pid::from_raw(pid.parse().unwrap()).unwrap();
            assert!(
                rustix::process::test_kill_process(raw).is_ok(),
                "{pid} gone"
            );
        }
        pids
    }

    /// Ask the server over the frozen handshake to move onto `binary`: the
    /// result it answers, `None` when the connection closes without one
    /// (the server execed).
    fn upgrade(&self, binary: &Path) -> Option<ServerMsg> {
        let path = self.run(&["display", "-p", "#{socket_path}"]);
        let mut stream = UnixStream::connect(path.trim_end()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let request = ClientMsg::UpgradeRequest {
            binary: binary.to_owned(),
        };
        protocol::write_msg(&mut stream, &request).unwrap();
        protocol::read_msg::<ServerMsg>(&mut stream).expect("an answer or the close in time")
    }

    /// The server's command line, from `ps` (portable to macOS).
    fn server_args(&self) -> String {
        let pid = self.pid();
        let out = Command::new("ps")
            .args(["-o", "args=", "-p", &pid])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Type a line into `pane` and wait for `cat` to echo it back.
    fn echo(&self, pane: &str, word: &str) {
        self.run(&["send-keys", "-t", pane, word, "Enter"]);
        wait("the pane echoing", || {
            self.run(&["capture-pane", "-p", "-t", pane])
                .matches(word)
                .count()
                >= 2
        });
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

fn wait(what: &str, mut done: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// An attached client under a PTY; what it was sent, as it comes.
struct Attached {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    bytes: Arc<Mutex<Vec<u8>>>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Attached {
    fn new(world: &World, session: &str) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(BIN);
        cmd.args([
            "-L",
            &world.label,
            "-f",
            "/dev/null",
            "attach",
            "-t",
            session,
        ]);
        cmd.env_clear();
        for (k, v) in world.env() {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&bytes);
        thread::spawn(move || {
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

    fn len(&self) -> usize {
        self.bytes.lock().unwrap().len()
    }

    fn since(&self, at: usize) -> String {
        String::from_utf8_lossy(&self.bytes.lock().unwrap()[at..]).into_owned()
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A control client (`-C attach`); its output lines as they come.
struct Control {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
}

impl Control {
    fn new(world: &World, session: &str) -> Self {
        let mut child = world
            .command(&["-C", "attach", "-t", session])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        thread::spawn(move || {
            for line in out.lines() {
                let Ok(line) = line else { break };
                sink.lock().unwrap().push(line);
            }
        });
        Self { child, lines }
    }

    fn has(&self, prefix: &str) -> bool {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with(prefix))
    }

    fn send(&mut self, line: &str) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The server's executable, when it is this build's.
fn bin() -> PathBuf {
    PathBuf::from(BIN)
}

/// The tentpole: the server execs itself with panes running, an attached
/// client and a control client. Same pid, the same pane processes, every
/// listing byte for byte; the panes, the attached client and the control
/// client go on working.
#[test]
fn a_server_execs_itself_and_everything_carries_on() {
    let world = World::new();
    world.populate(&[]);
    let attached = Attached::new(&world, "a");
    let mut control = Control::new(&world, "b");
    wait("both clients", || world.run(&["lsc"]).lines().count() == 2);
    let pid = world.pid();
    let panes = world.pane_pids();
    let before = world.observe();
    assert!(!world.server_args().contains("--vtmux-resume"));

    assert_eq!(world.upgrade(&bin()), None, "the server execed");

    assert_eq!(world.pid(), pid);
    assert!(
        world.server_args().contains("--vtmux-resume"),
        "{}",
        world.server_args()
    );
    assert_eq!(world.pane_pids(), panes);
    assert_eq!(world.observe(), before);

    // The image that resumed upgrades again, from where it stands. (Before
    // any pane output: pane terminal state is not carried yet.)
    assert_eq!(world.upgrade(&bin()), None, "the server execed again");
    assert_eq!(world.pid(), pid);
    assert_eq!(world.pane_pids(), panes);
    assert_eq!(world.observe(), before);

    // The panes, the attached client showing one of them, and the control
    // client.
    let b = world.run(&["display", "-p", "-t", "b", "#{pane_id}"]);
    let b = b.trim();
    let at = attached.len();
    world.echo("a:w1", "upgraded");
    wait("the attached client's redraw", || {
        attached.since(at).contains("upgraded")
    });
    world.echo("b:x", "ctl");
    wait("%output from the control client", || {
        control.has(&format!("%output {b} ctl"))
    });
    control.send("display -p '#{session_name}'");
    wait("a command over control mode", || control.has("b"));
    world.echo("a:w2.1", "split");
}

/// A binary that is no vtmux fails the dry run: the server stays this
/// image, untouched, and says why.
#[test]
fn a_binary_that_fails_the_dry_run_leaves_the_server_as_it_was() {
    let world = World::new();
    world.populate(&[]);
    let pid = world.pid();
    let before = world.observe();

    let garbage = world.home.path().join("garbage");
    std::fs::write(&garbage, [0x7f, 0x00, 0xff, 0x13, 0x37, 0x00]).unwrap();
    set_executable(&garbage);
    let Some(ServerMsg::UpgradeResult { ok: false, message }) = world.upgrade(&garbage) else {
        panic!("no refusal");
    };
    assert!(
        message.starts_with(&format!("{}: ", garbage.display())),
        "{message}"
    );

    // A binary that runs and refuses the snapshot: its message.
    let refuses = world.home.path().join("refuses");
    std::fs::write(
        &refuses,
        "#!/bin/sh\necho 'snapshot not for me' >&2\nexit 3\n",
    )
    .unwrap();
    set_executable(&refuses);
    assert_eq!(
        world.upgrade(&refuses),
        Some(ServerMsg::UpgradeResult {
            ok: false,
            message: "snapshot not for me".into(),
        })
    );

    assert_eq!(world.pid(), pid);
    assert!(!world.server_args().contains("--vtmux-resume"));
    assert_eq!(world.observe(), before);
    world.echo("a:w1", "still");
}

/// Decision 7: a resume that fails after the exec (the test hook) switches
/// back to the binary that wrote the snapshot, which carries on.
#[test]
fn a_resume_that_fails_switches_back() {
    let world = World::new();
    world.populate(&[("VTMUX_TEST_RESUME_FAIL", "1")]);
    let pid = world.pid();
    let panes = world.pane_pids();
    let before = world.observe();

    assert_eq!(world.upgrade(&bin()), None);

    assert_eq!(world.pid(), pid);
    let args = world.server_args();
    assert!(args.contains("--vtmux-switched-back"), "{args}");
    assert_eq!(world.pane_pids(), panes);
    assert_eq!(world.observe(), before);
    world.echo("b:x", "back");
}

/// When the image switched back to fails too, the panes are still served:
/// rebuilt without terminal state rather than lost.
#[test]
fn a_switch_back_that_fails_too_still_serves_the_panes() {
    let world = World::new();
    world.populate(&[("VTMUX_TEST_RESUME_FAIL", "always")]);
    let pid = world.pid();
    let panes = world.pane_pids();
    let before = world.observe();

    assert_eq!(world.upgrade(&bin()), None);

    assert_eq!(world.pid(), pid);
    assert!(world.server_args().contains("--vtmux-switched-back"));
    assert_eq!(world.pane_pids(), panes);
    assert_eq!(world.observe(), before);
    world.echo("a:w2.0", "last");
}

/// The dry run on its own: `--vtmux-check-snapshot` on a descriptor that
/// is no snapshot exits 1 with why, and on one that is not open too.
#[test]
fn the_dry_run_flag_refuses_what_is_no_snapshot() {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"not a snapshot").unwrap();
    let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
    // The child inherits it at the same number.
    rustix::io::fcntl_setfd(&file, rustix::io::FdFlags::empty()).unwrap();
    let out = Command::new(BIN)
        .args(["--vtmux-check-snapshot", &fd.to_string()])
        .env_clear()
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "not a vtmux snapshot\n"
    );

    let out = Command::new(BIN)
        .args(["--vtmux-check-snapshot", "250"])
        .env_clear()
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).starts_with("snapshot descriptor 250: "),
        "{out:?}"
    );
}

fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}
