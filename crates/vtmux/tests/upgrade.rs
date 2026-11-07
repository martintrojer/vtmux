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
    /// The server pid, once seen: the teardown kills it when it no
    /// longer answers.
    server: Mutex<Option<rustix::process::Pid>>,
    /// The socket path, once an upgrade asked for it.
    socket: Mutex<Option<PathBuf>>,
    /// The binary every client runs as: this build, or a link to it named
    /// `tmux` (the socket directory follows the name, `tmux-UID`).
    client: Mutex<PathBuf>,
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
            server: Mutex::new(None),
            socket: Mutex::new(None),
            client: Mutex::new(PathBuf::from(BIN)),
        }
    }

    fn client(&self) -> PathBuf {
        self.client.lock().unwrap().clone()
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
        self.command_of(&self.client(), args)
    }

    /// [`Self::command`] run by `binary`.
    fn command_of(&self, binary: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(binary);
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
            "{args:?}: {}, stderr {:?}, stdout {:?}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Two sessions, three windows, four `cat -` panes, and state of every
    /// kind the listings show. `server_env` goes to the server's process
    /// environment (the test hooks).
    fn populate(&self, server_env: &[(&str, &str)]) {
        self.populate_from(&self.client(), server_env);
    }

    /// [`Self::populate`] with the server started from `binary`.
    fn populate_from(&self, binary: &Path, server_env: &[(&str, &str)]) {
        let out = output_retrying(|| {
            let mut cmd = self.command_of(
                binary,
                &[
                    "new", "-d", "-s", "a", "-n", "w1", "-x", "80", "-y", "24", "cat", "-",
                ],
            );
            cmd.envs(server_env.iter().copied());
            cmd
        });
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
        let pid = self.run(&["display", "-p", "#{pid}"]).trim().to_owned();
        *self.server.lock().unwrap() = common::parse_pid(&pid);
        pid
    }

    /// A command's output, or `None` when it has not finished within
    /// `limit` (it is killed): for a server that may not answer.
    fn run_within(&self, args: &[&str], limit: Duration) -> Option<std::process::Output> {
        let mut child = self
            .command(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let end = Instant::now() + limit;
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= end {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        }
        Some(child.wait_with_output().unwrap())
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

    /// [`Self::observe`], taken again (up to 5 times) while it differs
    /// from `want`: tmux cuts a format expansion short after 100 ms
    /// (format.c `format_check_time`), which a loaded host can make a
    /// listing hit, leaving the end of a line out. A real difference stays.
    fn observe_like(&self, want: &str) -> String {
        let mut got = self.observe();
        for _ in 0..4 {
            if got == want {
                break;
            }
            thread::sleep(Duration::from_millis(100));
            got = self.observe();
        }
        got
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
    /// result it answers, from this image when it refuses, else from the
    /// image that ends up serving; `None` when the connection closes
    /// without one.
    fn upgrade(&self, binary: &Path) -> Option<ServerMsg> {
        let path = self.run(&["display", "-p", "#{socket_path}"]);
        *self.socket.lock().unwrap() = Some(PathBuf::from(path.trim_end()));
        self.upgrade_at(binary)
    }

    /// [`Self::upgrade`] on the socket an earlier one used, for a server
    /// with no session to expand `#{socket_path}` in.
    fn upgrade_at(&self, binary: &Path) -> Option<ServerMsg> {
        let path = self
            .socket
            .lock()
            .unwrap()
            .clone()
            .expect("an earlier upgrade");
        let mut stream = UnixStream::connect(path).unwrap();
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
        // Bounded: a server that no longer answers is killed by its pid.
        let limit = Duration::from_secs(10);
        let pid = self
            .run_within(&["display", "-p", "#{pid}"], limit)
            .and_then(|o| common::parse_pid(&String::from_utf8_lossy(&o.stdout)))
            .or(*self.server.lock().unwrap());
        let _ = self.run_within(&["kill-server"], limit);
        common::reap(pid);
    }
}

/// `make()`'s output, spawned again while the binary is busy: a binary
/// written a moment ago can still be open for writing in a child another
/// test thread forked (`ETXTBSY`).
fn output_retrying(make: impl Fn() -> Command) -> std::process::Output {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        match make().output() {
            Err(e)
                if e.raw_os_error() == Some(rustix::io::Errno::TXTBSY.raw_os_error())
                    && Instant::now() < end =>
            {
                thread::sleep(Duration::from_millis(50));
            }
            other => return other.unwrap(),
        }
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
    writer: Box<dyn std::io::Write + Send>,
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
        let mut cmd = CommandBuilder::new(world.client());
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
        let writer = pair.master.take_writer().unwrap();
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
            writer,
            _master: pair.master,
        }
    }

    /// The client's pid (an exec keeps it).
    fn pid(&self) -> String {
        self.child.process_id().unwrap().to_string()
    }

    /// The client's command line, from `ps` (portable to macOS).
    fn args(&self) -> String {
        let out = Command::new("ps")
            .args(["-o", "args=", "-p", &self.pid()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// What its terminal shows: everything it was sent, fed to a
    /// terminal of its size, captured with styles, and the cursor.
    fn screen(&self) -> String {
        let mut t = vtmux_vt::PaneTerm::new(80, 24, 0);
        t.feed(&self.bytes.lock().unwrap());
        let opts = vtmux_vt::CaptureOpts {
            escapes: true,
            ..vtmux_vt::CaptureOpts::default()
        };
        let text = String::from_utf8_lossy(&vtmux_vt::capture(&t, opts)).into_owned();
        format!("{text}cursor {:?}\n", t.cursor())
    }

    fn keys(&mut self, bytes: &str) {
        self.writer.write_all(bytes.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    /// The client's exit status, once it has exited.
    fn exited(&mut self) -> Option<u32> {
        self.child.try_wait().unwrap().map(|s| s.exit_code())
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

    fn send_raw(&mut self, bytes: &str) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(bytes.as_bytes()).unwrap();
        stdin.flush().unwrap();
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

/// The handshake's answer to an upgrade that went through.
fn upgraded() -> ServerMsg {
    ServerMsg::UpgradeResult {
        ok: true,
        message: String::new(),
    }
}

/// The message of a handshake's answer that the upgrade failed.
fn failed(answer: Option<ServerMsg>) -> String {
    match answer {
        Some(ServerMsg::UpgradeResult { ok: false, message }) => message,
        other => panic!("not a failure: {other:?}"),
    }
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

    assert_eq!(world.upgrade(&bin()), Some(upgraded()), "the server execed");

    assert_eq!(world.pid(), pid);
    assert!(
        world.server_args().contains("--vtmux-resume"),
        "{}",
        world.server_args()
    );
    assert_eq!(world.pane_pids(), panes);
    assert_eq!(world.observe_like(&before), before);

    // The image that resumed upgrades again, from where it stands. (Before
    // any pane output: pane terminal state is not carried yet.)
    assert_eq!(
        world.upgrade(&bin()),
        Some(upgraded()),
        "the server execed again"
    );
    assert_eq!(world.pid(), pid);
    assert_eq!(world.pane_pids(), panes);
    assert_eq!(world.observe_like(&before), before);

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

/// The upgrade as a user runs it (decision 22): the server was started
/// as `bin/tmux`, a link to this build; the link is moved to a new copy,
/// and `tmux vtmux-upgrade` with no `-b` moves the server onto the copy.
/// The command answers after the exec (exit 0, nothing printed); same
/// pid, the same pane processes, every listing byte for byte, and the
/// attached and control clients go on working.
#[test]
fn tmux_vtmux_upgrade_follows_a_relink_of_the_start_path() {
    let world = World::new();
    let dir = world.home.path().join("bin");
    std::fs::create_dir(&dir).unwrap();
    let link = dir.join("tmux");
    std::os::unix::fs::symlink(BIN, &link).unwrap();
    let copy = world.home.path().join("vtmux-new");
    std::fs::copy(BIN, &copy).unwrap();
    set_executable(&copy);
    *world.client.lock().unwrap() = link.clone();
    world.populate(&[]);
    let attached = Attached::new(&world, "a");
    let mut control = Control::new(&world, "b");
    wait("both clients", || world.run(&["lsc"]).lines().count() == 2);
    let pid = world.pid();
    let panes = world.pane_pids();
    let before = world.observe();
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&copy, &link).unwrap();

    let out = world
        .run_within(&["vtmux-upgrade"], Duration::from_secs(60))
        .expect("vtmux-upgrade answered");
    assert_eq!(
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).as_ref(),
            String::from_utf8_lossy(&out.stderr).as_ref()
        ),
        (Some(0), "", "")
    );

    assert_eq!(world.pid(), pid);
    let args = world.server_args();
    assert!(args.contains("--vtmux-resume"), "{args}");
    #[cfg(target_os = "linux")]
    assert_eq!(
        std::fs::read_link(format!("/proc/{pid}/exe")).unwrap(),
        std::fs::canonicalize(&copy).unwrap()
    );
    assert_eq!(world.pane_pids(), panes);
    assert_eq!(world.observe_like(&before), before);
    let b = world.run(&["display", "-p", "-t", "b", "#{pane_id}"]);
    let b = b.trim();
    let at = attached.len();
    world.echo("a:w1", "relinked");
    wait("the attached client's redraw", || {
        attached.since(at).contains("relinked")
    });
    world.echo("b:x", "ctl");
    wait("%output from the control client", || {
        control.has(&format!("%output {b} ctl"))
    });
    control.send("display -p 'after #{session_name}'");
    wait("a command over control mode", || control.has("after b"));
}

/// `vtmux-upgrade` from a control client (mu, murmur): its guard closes
/// as for any waiting command, and the line it sent behind it, held while
/// the upgrade ran, runs in the new image.
#[test]
fn a_control_client_upgrades_and_reads_on() {
    let world = World::new();
    world.populate(&[]);
    let mut control = Control::new(&world, "b");
    wait("the control client", || {
        world.run(&["lsc"]).lines().count() == 1
    });
    let pid = world.pid();
    let panes = world.pane_pids();
    let before = world.observe();
    // Both lines in one write, so the second is in the server's buffer
    // while the first runs.
    control.send_raw(&format!(
        "vtmux-upgrade -b '{BIN}'\ndisplay -p 'after #{{pid}}'\n"
    ));
    wait("the line after the upgrade", || {
        control.has(&format!("after {pid}"))
    });
    let args = world.server_args();
    assert!(args.contains("--vtmux-resume"), "{args}");
    assert_eq!(world.pane_pids(), panes);
    assert_eq!(world.observe_like(&before), before);
    assert!(!control.has("%error"));
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
    // Refused, naming the binary and why. The wording is the kernel's
    // (the exec error), not a shell's, so it is the same on every host.
    assert!(
        message.contains(&garbage.display().to_string()),
        "{message}"
    );
    assert!(message.contains("format"), "{message}");

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
    assert_eq!(world.observe_like(&before), before);
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

    // The requester hears it from the image switched back to.
    let message = failed(world.upgrade(&bin()));
    assert!(
        message.starts_with(&format!(
            "live upgrade failed: {BIN}: test hook; switched back to "
        )),
        "{message}"
    );

    assert_eq!(world.pid(), pid);
    let args = world.server_args();
    assert!(args.contains("--vtmux-switched-back"), "{args}");
    assert_eq!(world.pane_pids(), panes);
    assert_eq!(world.observe_like(&before), before);
    world.echo("b:x", "back");
    // The switch back says so, with why.
    let log = world.run(&["show-messages"]);
    assert!(
        log.contains(&format!(
            "live upgrade failed: {BIN}: test hook; switched back to "
        )),
        "{log}"
    );
}

/// When the image switched back to fails too, the panes are still served:
/// rebuilt without terminal state rather than lost, and the log says so.
#[test]
fn a_switch_back_that_fails_too_still_serves_the_panes() {
    let world = World::new();
    world.populate(&[("VTMUX_TEST_RESUME_FAIL", "always")]);
    let pid = world.pid();
    let panes = world.pane_pids();
    let before = world.observe();

    let message = failed(world.upgrade(&bin()));
    assert!(
        message.ends_with("; panes restored without their terminal state"),
        "{message}"
    );

    assert_eq!(world.pid(), pid);
    assert!(world.server_args().contains("--vtmux-switched-back"));
    assert_eq!(world.pane_pids(), panes);
    assert_eq!(world.observe_like(&before), before);
    world.echo("a:w2.0", "last");
    let log = world.run(&["show-messages"]);
    assert!(log.contains("which failed too: test hook"), "{log}");
    assert!(
        log.contains("; panes restored without their terminal state"),
        "{log}"
    );
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

/// Decision 23: `--vtmux-patch-stack` prints the binary's Ghostty patch
/// stack and exits 0, starting nothing (no socket appears), so an old
/// image can ask a new one before it writes history in Ghostty's form.
#[test]
fn the_patch_stack_flag_prints_the_ghostty_patch_stack() {
    let sockets = tempfile::Builder::new()
        .prefix("vtu")
        .tempdir_in("/tmp")
        .unwrap();
    let out = Command::new(BIN)
        .arg("--vtmux-patch-stack")
        .env_clear()
        .env("TMUX_TMPDIR", sockets.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{}\n", vtmux_vt::PATCH_STACK)
    );
    assert_eq!(std::fs::read_dir(sockets.path()).unwrap().count(), 0);
}

/// A signal storm through 20 upgrades: SIGHUP (caught and ignored) and
/// SIGUSR2 (caught, tmux's debug-log toggle) sent every 200 us, so some
/// land between `execve` and the new image's handlers. Neither may kill
/// the server or, through its PTYs closing, a pane (proc.c
/// `proc_set_signals` catches both in every image).
#[test]
fn a_signal_storm_through_twenty_upgrades_kills_nothing() {
    use rustix::process::{Pid, Signal, kill_process};
    use std::sync::atomic::{AtomicBool, Ordering};
    let world = World::new();
    world.populate(&[]);
    let pid = world.pid();
    let panes = world.pane_pids();
    let before = world.observe();
    let target = Pid::from_raw(pid.parse().unwrap()).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let storm = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut sent = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let sig = if sent.is_multiple_of(2) {
                    Signal::HUP
                } else {
                    Signal::USR2
                };
                if kill_process(target, sig).is_err() {
                    break;
                }
                sent += 1;
                thread::sleep(Duration::from_micros(200));
            }
            sent
        })
    };
    for i in 0..20 {
        assert_eq!(world.upgrade(&bin()), Some(upgraded()), "upgrade {i}");
        assert_eq!(world.pid(), pid, "upgrade {i}");
    }
    stop.store(true, Ordering::Relaxed);
    assert!(storm.join().unwrap() > 0);
    assert!(
        world.server_args().contains("--vtmux-resume"),
        "{}",
        world.server_args()
    );
    assert_eq!(world.pane_pids(), panes);
    assert_eq!(world.observe_like(&before), before);
    world.echo("a:w1", "stormed");
}

/// Decision 7's switch back lands on the image that wrote the snapshot,
/// not on whatever its path holds by then: here a binary from before the
/// feature (a script that exits 1) installed over it. The server and its
/// panes carry on.
#[test]
fn a_switch_back_ignores_what_the_old_path_holds_now() {
    let world = World::new();
    let old = world.home.path().join("old");
    if std::fs::hard_link(BIN, &old).is_err() {
        std::fs::copy(BIN, &old).unwrap();
    }
    world.populate_from(&old, &[("VTMUX_TEST_RESUME_FAIL", "1")]);
    let pid = world.pid();
    let panes = world.pane_pids();
    let before = world.observe();
    std::fs::remove_file(&old).unwrap();
    std::fs::write(&old, "#!/bin/sh\nexit 1\n").unwrap();
    set_executable(&old);

    let message = failed(world.upgrade(&bin()));
    assert!(message.starts_with("live upgrade failed: "), "{message}");

    assert_eq!(world.pid(), pid);
    // Linux switches back through the executable kept open; macOS has no
    // fexecve, so its dry run refuses the script and the new image serves
    // through the last resort.
    #[cfg(target_os = "linux")]
    {
        let args = world.server_args();
        assert!(args.contains("--vtmux-switched-back"), "{args}");
    }
    assert_eq!(world.pane_pids(), panes);
    assert_eq!(world.observe_like(&before), before);
    world.echo("b:x", "back");
}

/// A panic while the new image rebuilds the server (the test hook) is a
/// failed resume like any other: it switches back, and no pane dies with
/// the process.
#[test]
fn a_panic_in_the_new_image_switches_back() {
    let world = World::new();
    world.populate(&[("VTMUX_TEST_RESUME_FAIL", "panic")]);
    let pid = world.pid();
    let panes = world.pane_pids();
    let before = world.observe();

    let message = failed(world.upgrade(&bin()));
    assert!(message.contains("panicked: test hook"), "{message}");

    assert_eq!(world.pid(), pid);
    let args = world.server_args();
    assert!(args.contains("--vtmux-switched-back"), "{args}");
    assert_eq!(world.pane_pids(), panes);
    assert_eq!(world.observe_like(&before), before);
    world.echo("a:w2.1", "caught");
}

/// The dry run rebuilds the server, not only decodes the snapshot: a
/// rebuild that fails (the test hook, which fails it only in the dry
/// run) refuses the upgrade while the old image is intact.
#[test]
fn the_dry_run_rebuilds_the_server() {
    let world = World::new();
    world.populate(&[("VTMUX_TEST_RESUME_FAIL", "check")]);
    let pid = world.pid();
    let before = world.observe();

    let Some(ServerMsg::UpgradeResult { ok: false, message }) = world.upgrade(&bin()) else {
        panic!("not refused");
    };
    assert!(message.contains("test hook"), "{message}");

    assert_eq!(world.pid(), pid);
    assert!(!world.server_args().contains("--vtmux-resume"));
    assert_eq!(world.observe_like(&before), before);
    world.echo("a:w1", "intact");
}

/// When no image can read the snapshot (a wrapper damages it after the
/// dry run), the server that is left still accepts and answers commands,
/// still ignores SIGHUP, keeps every pane process alive and exits on
/// `kill-server`.
#[test]
fn a_server_left_without_a_snapshot_still_answers() {
    use rustix::process::{Pid, Signal, kill_process, test_kill_process};
    let world = World::new();
    world.populate(&[]);
    let pid = world.pid();
    let panes = world.pane_pids();
    let wrapper = world.home.path().join("damages");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nif [ \"$1\" = --vtmux-resume ]; then printf damage >>/dev/fd/$2; fi\n\
             exec '{BIN}' \"$@\"\n"
        ),
    )
    .unwrap();
    set_executable(&wrapper);

    let message = failed(world.upgrade(&wrapper));
    assert!(message.ends_with("; no session restored"), "{message}");

    let limit = Duration::from_secs(10);
    let ls = world.run_within(&["ls"], limit).expect("ls answered");
    assert!(ls.status.success(), "{ls:?}");
    let log = world
        .run_within(&["show-messages"], limit)
        .expect("show-messages answered");
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(log.contains("live upgrade failed: "), "{log}");
    // Another upgrade would close the panes' terminals, which no snapshot
    // names now: refused.
    let Some(ServerMsg::UpgradeResult { ok: false, message }) = world.upgrade_at(&wrapper) else {
        panic!("not refused");
    };
    assert!(message.contains("held from a failed upgrade"), "{message}");
    let server = Pid::from_raw(pid.parse().unwrap()).unwrap();
    kill_process(server, Signal::HUP).unwrap();
    let ls = world
        .run_within(&["ls"], limit)
        .expect("ls answered after SIGHUP");
    assert!(ls.status.success(), "{ls:?}");
    for p in &panes {
        let p = Pid::from_raw(p.parse().unwrap()).unwrap();
        assert!(test_kill_process(p).is_ok(), "pane {p:?} gone");
    }
    let killed = world
        .run_within(&["kill-server"], limit)
        .expect("kill-server answered");
    assert!(killed.status.success(), "{killed:?}");
    wait("the server to exit", || test_kill_process(server).is_err());
}

/// The server left without a snapshot keeps reading the pane terminals it
/// holds, discarding what comes: a pane printing without pause goes on
/// (its counter advances far past what a full terminal would let it
/// write), instead of blocking on a terminal nobody reads.
#[test]
fn the_panes_of_a_server_left_without_a_snapshot_keep_printing() {
    let world = World::new();
    world.populate(&[]);
    let count = world.home.path().join("count");
    // 16 KiB a round: a terminal nobody reads stops it within a few.
    let printer = format!(
        "i=0; while :; do i=$((i+1)); echo $i >'{}'; \
         dd if=/dev/zero bs=4096 count=4 2>/dev/null; done",
        count.display()
    );
    world.run(&["neww", "-d", "-t", "a", "-n", "pr", "sh", "-c", &printer]);
    let counted = || {
        std::fs::read_to_string(&count)
            .ok()
            .and_then(|c| c.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    wait("the printer to start", || counted() > 2);
    let wrapper = world.home.path().join("damages");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nif [ \"$1\" = --vtmux-resume ]; then printf damage >>/dev/fd/$2; fi\n\
             exec '{BIN}' \"$@\"\n"
        ),
    )
    .unwrap();
    set_executable(&wrapper);

    let message = failed(world.upgrade(&wrapper));
    assert!(message.ends_with("; no session restored"), "{message}");

    let limit = Duration::from_secs(10);
    let log = world
        .run_within(&["show-messages"], limit)
        .expect("show-messages answered");
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(log.contains("live upgrade failed: "), "{log}");
    let at = counted();
    wait("the printer to go on", || counted() > at + 100);
    let ls = world.run_within(&["ls"], limit).expect("ls answered");
    assert!(ls.status.success(), "{ls:?}");
}

/// The dry run of a server that never upgraded inherits the snapshot and
/// nothing else: the listening socket and the pane PTYs are close-on-exec
/// from the start, so every upgrade starts its children alike.
#[test]
fn the_dry_run_inherits_only_the_snapshot() {
    let world = World::new();
    world.populate(&[]);
    let seen = world.home.path().join("fds");
    let lister = world.home.path().join("lister");
    std::fs::write(
        &lister,
        format!(
            "#!/bin/sh\nfor n in 3 4 5 6 7 8 9; do\n\
             [ \"$n\" = \"$2\" ] && continue\n\
             if (: <&\"$n\") 2>/dev/null; then echo \"$n\"; fi\n\
             done >'{}'\necho listed >&2\nexit 1\n",
            seen.display()
        ),
    )
    .unwrap();
    set_executable(&lister);

    assert_eq!(
        world.upgrade(&lister),
        Some(ServerMsg::UpgradeResult {
            ok: false,
            message: "listed".into(),
        })
    );
    assert_eq!(std::fs::read_to_string(&seen).unwrap(), "");
}

/// The same on Linux for every descriptor, with an attached client, a
/// control client and a `pipe-pane` running: the dry run holds nothing of
/// the server's but the snapshot (the review of `lu_upgrade_cmd`: a dry run that
/// inherited the listener or a pty master, and a descendant of it, could
/// keep them past its end).
#[cfg(target_os = "linux")]
#[test]
fn the_dry_run_inherits_nothing_of_the_server_on_linux() {
    let world = World::new();
    world.populate(&[]);
    let _attached = Attached::new(&world, "a");
    let _control = Control::new(&world, "b");
    wait("both clients", || world.run(&["lsc"]).lines().count() == 2);
    world.run(&["pipe-pane", "-t", "a:w1", "cat >/dev/null"]);
    let seen = world.home.path().join("fds");
    let snapshot = world.home.path().join("snapshot-fd");
    let lister = world.home.path().join("lister");
    // `ls` lists the shell's descriptors from outside, so none of its own
    // (a redirection, a command substitution) is among them.
    std::fs::write(
        &lister,
        format!(
            "#!/bin/sh\necho \"$2\" >'{}'\nls -l /proc/$$/fd >'{}'\necho listed >&2\nexit 1\n",
            snapshot.display(),
            seen.display()
        ),
    )
    .unwrap();
    set_executable(&lister);

    assert_eq!(failed(world.upgrade(&lister)), "listed");
    let snapshot = std::fs::read_to_string(&snapshot).unwrap();
    let script = lister.display().to_string();
    let inherited: Vec<String> = std::fs::read_to_string(&seen)
        .unwrap()
        .lines()
        .filter_map(|l| {
            let (left, target) = l.split_once(" -> ")?;
            let fd: u32 = left.rsplit(' ').next()?.parse().ok()?;
            // The shell's own: its script, and (dash) a copy of its
            // stdio, which the dry run gets as /dev/null.
            let shells = target == script || target == "/dev/null";
            (fd > 2 && fd.to_string() != snapshot.trim() && !shells)
                .then(|| format!("{fd} -> {target}"))
        })
        .collect();
    assert_eq!(inherited, Vec::<String>::new());
}

/// A dry run that does not end within its limit (3 s, so the server it
/// holds still is never frozen long) is refused with why, and killed with
/// every process it started: none is left holding the snapshot.
#[test]
fn a_dry_run_past_its_limit_is_killed_with_its_children() {
    let world = World::new();
    world.populate(&[]);
    let child = world.home.path().join("child");
    let slow = world.home.path().join("slow");
    std::fs::write(
        &slow,
        format!(
            "#!/bin/sh\nsleep 1000 &\necho $! >'{}'\nexec sleep 1000\n",
            child.display()
        ),
    )
    .unwrap();
    set_executable(&slow);

    assert_eq!(
        failed(world.upgrade(&slow)),
        format!("{} --vtmux-check-snapshot took over 3 s", slow.display())
    );
    assert_gone(&child);
    world.echo("a:w1", "unfrozen");
}

/// A dry run that refuses but leaves a child holding its stderr is
/// answered as soon as it exits, not when the child does, and the child
/// is killed.
#[test]
fn a_dry_run_whose_child_holds_its_stderr_answers_when_it_exits() {
    let world = World::new();
    world.populate(&[]);
    let child = world.home.path().join("child");
    let refuses = world.home.path().join("refuses");
    std::fs::write(
        &refuses,
        format!(
            "#!/bin/sh\nsleep 1000 &\necho $! >'{}'\necho 'not mine' >&2\nexit 1\n",
            child.display()
        ),
    )
    .unwrap();
    set_executable(&refuses);

    assert_eq!(failed(world.upgrade(&refuses)), "not mine");
    assert_gone(&child);
    world.echo("a:w1", "answered");
}

/// The process whose pid is in `file` has gone (it may linger a moment as
/// a zombie until whoever inherited it reaps it).
fn assert_gone(file: &Path) {
    use rustix::process::{Pid, test_kill_process};
    let pid: i32 = std::fs::read_to_string(file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let pid = Pid::from_raw(pid).unwrap();
    let zombie_or_gone = || {
        test_kill_process(pid).is_err()
            || Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.as_raw_nonzero().to_string()])
                .output()
                .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim().starts_with('Z'))
    };
    wait("the dry run's child to be killed", zombie_or_gone);
}

fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Every attached client as `lsc` shows it, with what decision 21 keeps:
/// pid, creation time, flags, key table, session, size and terminal.
fn clients(world: &World) -> String {
    world.run(&[
        "lsc",
        "-F",
        "#{client_name} #{client_pid} #{client_created} #{client_flags} \
         #{client_key_table} #{client_session} #{client_width}x#{client_height} \
         #{client_termname} #{client_utf8} #{client_readonly}",
    ])
}

/// The client has re-executed with a re-attach since its terminal had
/// taken `at` bytes, and is back: its terminal was restored (the
/// alternate screen left) and then started again, which only a re-attach
/// with the token does. Its command line, now the re-attach's.
fn reexecuted(attached: &Attached, at: usize) -> String {
    let mut now = String::new();
    wait("the client to re-execute", || {
        let since = attached.since(at);
        now = attached.args();
        now.contains(protocol::REATTACH_FLAG)
            && since
                .find("\u{1b}[?1049l")
                .is_some_and(|i| since[i..].contains("\u{1b}[?996n"))
    });
    now
}

/// `fresh`'s screen, once it has drawn its status line, and `attached`'s
/// equal to it.
fn same_screen_as_fresh(attached: &Attached, fresh: &Attached) {
    wait("the fresh client's first draw", || {
        fresh.screen().contains("w2")
    });
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let (a, f) = (attached.screen(), fresh.screen());
        if a == f {
            return;
        }
        assert!(
            Instant::now() < end,
            "re-attached:\n{a}\nfresh attach:\n{f}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Decisions 4 and 21 (live-upgrade spec, *Clients*): on the upgrade an
/// attached client re-executes the new binary (same process, its command
/// line now the re-attach) and comes back as the client it was: `lsc`
/// alike, no `client-*` hook and no `%client-*` notification, its screen
/// what a fresh attach shows, and its keys reach its pane.
#[test]
fn an_attached_client_re_executes_and_comes_back_as_it_was() {
    let world = World::new();
    world.populate(&[]);
    world.run(&["set", "-g", "@fired", "0"]);
    for hook in [
        "client-attached",
        "client-detached",
        "client-session-changed",
        "client-resized",
        "client-active",
        "client-focus-in",
        "client-focus-out",
    ] {
        world.run(&["set-hook", "-g", hook, &format!("set -ga @fired ' {hook}'")]);
    }
    let mut attached = Attached::new(&world, "a");
    let control = Control::new(&world, "b");
    wait("both clients", || world.run(&["lsc"]).lines().count() == 2);
    // Something on the old screen that the new image's panes don't show
    // until pane terminal state is carried: the redraw must cover it.
    world.echo("a:w1", "before");
    wait("the attached client's draw", || {
        attached.screen().contains("before")
    });
    let pid = attached.pid();
    let listed = clients(&world);
    let fired = world.run(&["show", "-gv", "@fired"]);
    let notified = |c: &Control| {
        c.lines
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with("%client-"))
            .cloned()
            .collect::<Vec<_>>()
    };
    let before = notified(&control);

    let at = attached.len();
    assert_eq!(world.upgrade(&bin()), Some(upgraded()));
    let now = reexecuted(&attached, at);
    // The token is not on the command line, where any user's `ps` shows
    // it: `<argv0> --vtmux-reattach <socket>`.
    let words: Vec<&str> = now.split_whitespace().collect();
    assert_eq!(
        (words.len(), words.get(1).copied()),
        (3, Some(protocol::REATTACH_FLAG)),
        "{now}"
    );
    assert_eq!(attached.pid(), pid, "the same process");
    wait("the terminal started again", || {
        attached.since(at).contains("\u{1b}[?996n")
    });
    assert_eq!(world.run(&["show", "-gv", "@fired"]), fired, "no hook");
    assert_eq!(notified(&control), before, "no notification");
    assert_eq!(clients(&world), listed);
    let fresh = Attached::new(&world, "a");
    same_screen_as_fresh(&attached, &fresh);
    drop(fresh);
    wait("the fresh client to go", || {
        world.run(&["lsc"]).lines().count() == 2
    });
    assert_eq!(clients(&world), listed);

    attached.keys("typed\r");
    let at = attached.len();
    wait("the keys to reach the pane", || {
        world
            .run(&["capture-pane", "-p", "-t", "a:w1"])
            .contains("typed")
    });
    wait("the echo on the client", || {
        attached.since(at).contains("typed")
    });
    assert!(attached.exited().is_none());
}

/// Twenty upgrades in a row, alternating between this build and a copy of
/// it at another path: after each the attached client has re-executed the
/// binary it was told to, is the same client (`lsc` alike, the same
/// process) and shows what a fresh attach shows.
#[test]
fn an_attached_client_follows_twenty_upgrades_in_a_row() {
    let world = World::new();
    world.populate(&[]);
    let copy = world.home.path().join("vtmux-copy");
    std::fs::copy(BIN, &copy).unwrap();
    set_executable(&copy);
    let attached = Attached::new(&world, "a");
    wait("the client", || world.run(&["lsc"]).lines().count() == 1);
    world.echo("a:w1", "first");
    let pid = attached.pid();
    let listed = clients(&world);
    for i in 0..20 {
        let binary = if i % 2 == 0 { copy.clone() } else { bin() };
        let at = attached.len();
        assert_eq!(world.upgrade(&binary), Some(upgraded()), "upgrade {i}");
        let args = reexecuted(&attached, at);
        assert!(
            args.contains(&format!(" {} ", protocol::REATTACH_FLAG)),
            "upgrade {i}: {args}"
        );
        assert_eq!(attached.pid(), pid, "upgrade {i}");
        wait("the terminal started again", || {
            attached.since(at).contains("\u{1b}[?996n")
        });
        let fresh = Attached::new(&world, "a");
        same_screen_as_fresh(&attached, &fresh);
        drop(fresh);
        wait("the fresh client to go", || {
            world.run(&["lsc"]).lines().count() == 1
        });
        assert_eq!(clients(&world), listed, "upgrade {i}");
        #[cfg(target_os = "linux")]
        assert_eq!(
            std::fs::read_link(format!("/proc/{pid}/exe")).unwrap(),
            std::fs::canonicalize(&binary).unwrap(),
            "upgrade {i}: the client runs the binary it was told to"
        );
    }
    // Each upgrade pops the title it pushes (`tty_stop_tty` before the
    // notice, `tty_start_tty` at the re-attach): one push, the attach's.
    let (pushed, popped) = titles(&attached);
    assert_eq!(pushed, popped + 1, "title pushes and pops");
    // A detach after them all leaves the terminal as it was.
    world.run(&["detach-client", "-s", "a"]);
    wait("the detach", || attached.since(0).contains("[detached"));
    assert_eq!(
        modes_left_on(&attached.bytes.lock().unwrap()),
        Vec::<String>::new()
    );
    assert_eq!(titles(&attached), (pushed, pushed));
}

/// The xterm title stack pushes (`CSI 22;0;0t`) and pops (`CSI 23;0;0t`)
/// a client's terminal was sent.
fn titles(attached: &Attached) -> (usize, usize) {
    let text = attached.since(0);
    (
        text.matches("\u{1b}[22;0;0t").count(),
        text.matches("\u{1b}[23;0;0t").count(),
    )
}

/// The terminal modes `bytes` leave on, of those a server puts a client's
/// terminal in: DEC private modes (the alternate screen, mouse, bracketed
/// paste, focus reports, cursor keys, theme reports; not 25, the cursor
/// shown, which is how a terminal starts), the keypad (`ESC =`) and
/// xterm's modified keys (`CSI > 4 ; n m`). Whatever the last sequence
/// for each did.
fn modes_left_on(bytes: &[u8]) -> Vec<String> {
    let mut on = std::collections::BTreeMap::<String, bool>::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            Some(b'=') => {
                on.insert("keypad".into(), true);
                i += 2;
            }
            Some(b'>') => {
                on.insert("keypad".into(), false);
                i += 2;
            }
            Some(b'[') => {
                let mut j = i + 2;
                let prefix = bytes.get(j).copied().filter(|b| b"?><=".contains(b));
                if prefix.is_some() {
                    j += 1;
                }
                let start = j;
                while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b';') {
                    j += 1;
                }
                let params =
                    String::from_utf8_lossy(&bytes[start..j.min(bytes.len())]).into_owned();
                let Some(&end) = bytes.get(j) else { break };
                let list: Vec<&str> = params.split(';').collect();
                match (prefix, end) {
                    (Some(b'?'), b'h' | b'l') => {
                        for p in list.iter().filter(|p| !p.is_empty() && **p != "25") {
                            on.insert(format!("?{p}"), end == b'h');
                        }
                    }
                    (Some(b'>'), b'm') if list.first() == Some(&"4") => {
                        let level = list.get(1).copied().unwrap_or("0");
                        on.insert("modifyOtherKeys".into(), level != "0" && !level.is_empty());
                    }
                    _ => {}
                }
                i = j + 1;
            }
            _ => i += 1,
        }
    }
    on.into_iter().filter(|(_, v)| *v).map(|(k, _)| k).collect()
}

/// A binary to upgrade onto: this build, except as `case` says for the
/// command lines it is given (a `sh` `case` body on `"$1"`, `$BIN` this
/// build).
fn wrapper(world: &World, name: &str, case: &str) -> PathBuf {
    let path = world.home.path().join(name);
    std::fs::write(
        &path,
        format!("#!/bin/sh\nBIN='{BIN}'\ncase \"$1\" in\n{case}\nesac\nexec \"$BIN\" \"$@\"\n"),
    )
    .unwrap();
    set_executable(&path);
    path
}

/// The upgrade onto `binary` with `attached` attached, asked from another
/// thread while this one reaps the client once it exits (a process that
/// has exited and is not reaped still exists): the answer, and the
/// client's exit status.
fn upgrade_losing(
    world: &World,
    binary: &Path,
    attached: &mut Attached,
) -> (Option<ServerMsg>, u32) {
    let path = world.run(&["display", "-p", "#{socket_path}"]);
    *world.socket.lock().unwrap() = Some(PathBuf::from(path.trim_end()));
    thread::scope(|scope| {
        let asked = scope.spawn(|| world.upgrade_at(binary));
        let mut status = None;
        wait("the client to exit", || {
            status = attached.exited();
            status.is_some()
        });
        (asked.join().unwrap(), status.unwrap())
    })
}

/// The review finding of `medium_reexec_teardown`: an attached client
/// told to re-execute that can't re-attach gets its terminal back as a
/// detach leaves it (no alternate screen, mouse, bracketed paste, focus
/// reports or keypad left on), says why, and the requester is warned,
/// naming it; the session is unharmed. `case` makes the binary fail the
/// re-attach.
fn a_lost_client_gets_its_terminal_back(case: &str, says: &str) {
    let world = World::new();
    world.populate(&[]);
    let mut attached = Attached::new(&world, "a");
    wait("the client", || world.run(&["lsc"]).lines().count() == 1);
    world.echo("a:w1", "before");
    wait("the client's draw", || attached.screen().contains("before"));
    assert!(
        modes_left_on(&attached.bytes.lock().unwrap()).contains(&"?1049".to_owned()),
        "attached, the terminal is on the alternate screen"
    );
    let name = world.run(&["lsc", "-F", "#{client_name}"]);
    let binary = wrapper(&world, "vtmux-new", case);
    let (answer, status) = upgrade_losing(&world, &binary, &mut attached);
    assert_eq!(
        answer,
        Some(ServerMsg::UpgradeResult {
            ok: true,
            message: format!(
                "warning: client {} did not re-attach after the upgrade: its session is \
                 still there, attach again",
                name.trim_end()
            ),
        })
    );
    assert_eq!(status, 1);
    assert_eq!(
        modes_left_on(&attached.bytes.lock().unwrap()),
        Vec::<String>::new()
    );
    let (pushed, popped) = titles(&attached);
    assert_eq!(pushed, popped, "the title popped");
    wait("why, on the terminal", || attached.since(0).contains(says));
    assert!(world.server_args().contains("--vtmux-resume"));
    assert_eq!(world.run(&["lsc"]), "");
    world.echo("a:w1", "after");
}

/// A binary from before clients followed upgrades (a downgrade) doesn't
/// know the re-attach command line.
#[test]
fn a_client_whose_new_binary_cannot_re_attach_gets_its_terminal_back() {
    a_lost_client_gets_its_terminal_back(
        "--vtmux-reattach) echo 'usage: vtmux [-2CDlNuVv]' >&2; exit 1;;",
        "usage: vtmux",
    );
}

/// The new image refuses the token the client presents.
#[test]
fn a_client_whose_token_is_refused_gets_its_terminal_back() {
    a_lost_client_gets_its_terminal_back(
        "--vtmux-reattach) VTMUX_REATTACH_TOKEN=0123456789abcdef; \
         export VTMUX_REATTACH_TOKEN;;",
        "[server upgraded: re-attach]",
    );
}

/// The client's exec of the new binary fails (here the binary stops being
/// executable once its dry run has passed, so the server's `execve` fails
/// too, after the notices): the client gets its terminal back and says
/// why; the requester hears the upgrade failed, and the old image serves
/// on.
#[test]
fn a_client_whose_exec_fails_gets_its_terminal_back() {
    let world = World::new();
    world.populate(&[]);
    let mut attached = Attached::new(&world, "a");
    wait("the client", || world.run(&["lsc"]).lines().count() == 1);
    world.echo("a:w1", "before");
    wait("the client's draw", || attached.screen().contains("before"));
    let pid = world.pid();
    let binary = wrapper(
        &world,
        "vtmux-new",
        "--vtmux-check-snapshot) chmod -x \"$0\";;",
    );
    let (answer, status) = upgrade_losing(&world, &binary, &mut attached);
    let why = failed(answer);
    assert!(why.starts_with(&binary.display().to_string()), "{why}");
    assert_eq!(status, 1);
    assert_eq!(
        modes_left_on(&attached.bytes.lock().unwrap()),
        Vec::<String>::new()
    );
    wait("why, on the terminal", || {
        attached
            .since(0)
            .contains("can't follow the server's upgrade")
    });
    assert_eq!(world.pid(), pid);
    assert!(!world.server_args().contains("--vtmux-resume"));
    wait("the client given up on", || world.run(&["lsc"]).is_empty());
    world.echo("a:w1", "after");
}

/// `%output` payloads of pane `pane` in a control client's lines, octal
/// escapes undone (control.c `control_append_data`), in order.
fn output_of(lines: &[String], pane: &str) -> Vec<u8> {
    let prefix = format!("%output {pane} ");
    let mut out = Vec::new();
    for line in lines {
        let Some(data) = line.strip_prefix(&prefix) else {
            continue;
        };
        let b = data.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(u8::is_ascii_digit) {
                let n = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
                out.push(n);
                i += 4;
            } else if b[i] == b'\\' && b.get(i + 1) == Some(&b'\\') {
                out.push(b'\\');
                i += 2;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    out
}

/// Decisions 9 and 26: two control clients stay connected through three
/// upgrades while a pane streams numbered lines, and each gets every line
/// exactly once, in order, with nothing lost or doubled at the exec.
#[test]
fn control_clients_stream_on_through_upgrades_without_loss_or_repeat() {
    let world = World::new();
    world.populate(&[]);
    let c1 = Control::new(&world, "b");
    let c2 = Control::new(&world, "b");
    wait("both control clients", || {
        world.run(&["lsc"]).lines().count() == 2
    });
    let stop = world.home.path().join("stop");
    let printer = format!(
        "i=0; while [ ! -e '{}' ]; do i=$((i+1)); echo n$i; \
         if [ $((i % 200)) = 0 ]; then sleep 0.01; fi; done; echo END; exec cat -",
        stop.display()
    );
    world.run(&["neww", "-d", "-t", "b", "-n", "pr", "sh", "-c", &printer]);
    let pane = world.run(&["display", "-p", "-t", "b:pr", "#{pane_id}"]);
    let pane = pane.trim();
    let seen = |c: &Control| output_of(&c.lines.lock().unwrap(), pane);
    wait("the stream to start", || {
        seen(&c1).len() > 1000 && seen(&c2).len() > 1000
    });
    for i in 0..3 {
        assert_eq!(world.upgrade(&bin()), Some(upgraded()), "upgrade {i}");
        let (a, b) = (seen(&c1).len(), seen(&c2).len());
        wait("the stream to go on", || {
            seen(&c1).len() > a + 1000 && seen(&c2).len() > b + 1000
        });
    }
    std::fs::write(&stop, "").unwrap();
    let ended = |c: &Control| seen(c).ends_with(b"END\r\n");
    wait("the end of the stream", || ended(&c1) && ended(&c2));
    for (name, c) in [("first", &c1), ("second", &c2)] {
        let text = String::from_utf8(seen(c)).unwrap();
        let lines: Vec<&str> = text.split_terminator("\r\n").collect();
        let (last, numbers) = lines.split_last().unwrap();
        assert_eq!(*last, "END", "{name}");
        assert!(numbers.len() > 500, "{name}: {} lines", numbers.len());
        for (i, line) in numbers.iter().enumerate() {
            assert_eq!(*line, format!("n{}", i + 1), "{name} client, line {i}");
        }
    }
    assert!(!c1.has("%exit") && !c2.has("%exit"));
}

/// A raw attached client speaking this protocol, as a binary from before
/// clients followed upgrades (`follows` false: no `follows_upgrade` in its
/// `Hello`) or as one that says it follows and then ignores its notice;
/// what it is sent, as it comes. It never acknowledges a frame.
struct Raw {
    _stream: UnixStream,
    got: Arc<Mutex<Vec<ServerMsg>>>,
}

impl Raw {
    fn new(world: &World, tty: &str, follows: bool) -> Self {
        let path = world.run(&["display", "-p", "#{socket_path}"]);
        let mut stream = UnixStream::connect(path.trim_end()).unwrap();
        let hello = protocol::Hello {
            version: protocol::PROTOCOL_VERSION,
            invocation: vtmux_core::socket::Invocation::Vtmux,
            pid: std::process::id(),
            argv: vec![b"attach".to_vec(), b"-t".to_vec(), b"a".to_vec()],
            cwd: world.home.path().to_path_buf(),
            env: vec![("TERM".into(), "xterm-256color".into())],
            tty: Some(protocol::TtyInfo {
                name: tty.into(),
                term: "xterm-256color".into(),
                cols: 80,
                rows: 24,
                features: Vec::new(),
            }),
            flags: protocol::ClientFlags::default(),
            ext: protocol::HelloExt {
                follows_upgrade: follows,
                reattach: None,
            },
        };
        protocol::write_msg(&mut stream, &ClientMsg::Hello(hello)).unwrap();
        let mut reader = stream.try_clone().unwrap();
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&got);
        thread::spawn(move || {
            while let Ok(Some(m)) = protocol::read_msg::<ServerMsg>(&mut reader) {
                sink.lock().unwrap().push(m);
            }
        });
        Self {
            _stream: stream,
            got,
        }
    }

    fn exit(&self) -> Option<ServerMsg> {
        self.got
            .lock()
            .unwrap()
            .iter()
            .find(|m| matches!(m, ServerMsg::Exit { .. }))
            .cloned()
    }

    fn noticed(&self) -> bool {
        self.got
            .lock()
            .unwrap()
            .iter()
            .any(|m| matches!(m, ServerMsg::UpgradeNotice { .. }))
    }
}

/// Decisions 4 and 25: a client from before clients followed upgrades is
/// detached by the upgrade with `[server upgraded: re-attach]`, and the
/// requester is warned, naming it; the upgrade goes on and a client of
/// this build follows. A client that is told to re-execute and doesn't
/// is detached the same way once its time to re-attach is up.
#[test]
fn a_client_that_does_not_follow_is_detached_and_named_to_the_requester() {
    let world = World::new();
    world.populate(&[]);
    let attached = Attached::new(&world, "a");
    let old = Raw::new(&world, "/dev/vtu-old", false);
    let deaf = Raw::new(&world, "/dev/vtu-deaf", true);
    wait("three clients", || world.run(&["lsc"]).lines().count() == 3);
    let at = attached.len();

    // The answer waits for the clients told to re-attach: this build's
    // comes back, the deaf one is given up on after its 10 s.
    assert_eq!(
        world.upgrade(&bin()),
        Some(ServerMsg::UpgradeResult {
            ok: true,
            message: "warning: client /dev/vtu-old can't follow an upgrade: detached \
                      with [server upgraded: re-attach]\n\
                      warning: client /dev/vtu-deaf did not act on the upgrade: detached \
                      with [server upgraded: re-attach]"
                .into(),
        })
    );
    let detached = Some(ServerMsg::Exit {
        code: 1,
        message: Some("server upgraded: re-attach".into()),
        hup: false,
    });
    wait("the old client's detach", || old.exit().is_some());
    assert_eq!(old.exit(), detached);
    reexecuted(&attached, at);
    assert!(deaf.noticed());
    wait("the deaf client's detach", || deaf.exit().is_some());
    assert_eq!(deaf.exit(), detached);
    wait("the clients left", || {
        world.run(&["lsc", "-F", "#{client_name}"]).lines().count() == 1
    });
    let left = world.run(&["lsc", "-F", "#{client_name}"]);
    assert!(!left.contains("/dev/vtu-"), "{left}");
}

/// The first index at or after `from` where `needle` starts in `hay`.
fn find_from(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    hay.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| from + i)
}

/// Ten panes in session `s` printing `n1`, `n2`, ... until `stop` exists,
/// then `END`: two in window `w0` (beside its `cat -`) and two in each of
/// `w1` to `w4`. Their ids.
fn ten_streams(world: &World, stop: &Path) -> Vec<String> {
    let printer = format!(
        "i=0; while [ ! -e '{}' ]; do i=$((i+1)); echo n$i; \
         if [ $((i % 50)) = 0 ]; then sleep 0.01; fi; done; echo END; exec cat -",
        stop.display()
    );
    for w in 0..5 {
        let window = format!("w{w}");
        if w > 0 {
            let args = ["neww", "-d", "-t", "s", "-n", &window, "sh", "-c", &printer];
            world.run(&args);
        } else {
            world.run(&["splitw", "-d", "-t", "s:w0", "sh", "-c", &printer]);
        }
        let target = format!("s:{window}");
        world.run(&["splitw", "-d", "-t", &target, "sh", "-c", &printer]);
    }
    let format = "#{pane_id} #{pane_start_command}";
    let streaming: Vec<String> = world
        .run(&["lsp", "-s", "-t", "s", "-F", format])
        .lines()
        .filter(|l| l.contains("stop"))
        .map(|l| l.split(' ').next().unwrap().to_owned())
        .collect();
    assert_eq!(streaming.len(), 10, "{streaming:?}");
    streaming
}

/// How long after `start` the client, sent `at` bytes then, got its full
/// redraw: its terminal started again (`Caps::restart`), then the status
/// line's `mark`.
fn redrawn_after(attached: &Attached, at: usize, mark: &str, start: Instant) -> Duration {
    loop {
        let bytes = attached.bytes.lock().unwrap();
        if let Some(i) = find_from(&bytes, at, b"\x1b[?996n")
            && find_from(&bytes, i, mark.as_bytes()).is_some()
        {
            return start.elapsed();
        }
        drop(bytes);
        assert!(start.elapsed() < Duration::from_secs(20), "no redraw");
        thread::sleep(Duration::from_millis(1));
    }
}

/// Control client `c` got pane `p`'s `n1` to `nN` and `END`, each once and
/// in order.
fn every_line_once(c: &Control, p: &str, name: &str) {
    let text = String::from_utf8(output_of(&c.lines.lock().unwrap(), p)).unwrap();
    let lines: Vec<&str> = text.split_terminator("\r\n").collect();
    let (last, numbers) = lines.split_last().unwrap();
    assert_eq!(*last, "END", "{name} client, {p}");
    for (i, line) in numbers.iter().enumerate() {
        assert_eq!(*line, format!("n{}", i + 1), "{name} client, {p}, line {i}");
    }
}

/// Decision 13 under load (live-upgrade spec, *Tests*): ten panes stream
/// numbered lines, one attached client shows two of them and two control
/// clients read every pane's `%output`, and the server upgrades onto
/// itself mid-stream. No process is restarted (the server, every pane and
/// the client keep their pids), each control client gets every line of
/// every pane exactly once and in order, and once the streams end the
/// client's screen is what a fresh attach shows. The time from the request
/// to the client's redraw is printed against the 1 s target (a debug
/// build on a loaded host: recorded, never asserted; `scripts/bench.py
/// upgrade` measures a release build).
#[test]
fn an_upgrade_under_load_loses_and_repeats_nothing() {
    const MARK: &str = "Q9Q";
    let world = World::new();
    let out = output_retrying(|| {
        world.command(&[
            "new", "-d", "-s", "s", "-n", "w0", "-x", "80", "-y", "24", "cat", "-",
        ])
    });
    assert!(out.status.success(), "{out:?}");
    world.run(&["set", "-g", "status-right", MARK]);
    world.run(&["select-pane", "-t", "s:w0", "-T", "t"]);
    let c1 = Control::new(&world, "s");
    let c2 = Control::new(&world, "s");
    let attached = Attached::new(&world, "s");
    wait("three clients", || world.run(&["lsc"]).lines().count() == 3);
    let stop = world.home.path().join("stop");
    let streaming = ten_streams(&world, &stop);
    let seen = |c: &Control, p: &str| output_of(&c.lines.lock().unwrap(), p).len();
    let all_past = |at: &[(usize, usize)], more: usize| {
        streaming
            .iter()
            .zip(at)
            .all(|(p, &(a, b))| seen(&c1, p) > a + more && seen(&c2, p) > b + more)
    };
    // Some 20 KB a pane, so a good deal is in flight at the upgrade.
    wait("every pane streaming", || {
        all_past(&vec![(0, 0); streaming.len()], 20_000)
    });

    let pid = world.pid();
    let panes = world.pane_pids();
    let client = attached.pid();
    let at = attached.len();
    let before: Vec<(usize, usize)> = streaming
        .iter()
        .map(|p| (seen(&c1, p), seen(&c2, p)))
        .collect();
    let start = Instant::now();
    assert_eq!(world.upgrade(&bin()), Some(upgraded()), "the server execed");
    let answered = start.elapsed();
    let redrawn = redrawn_after(&attached, at, MARK, start);
    eprintln!(
        "upgrade under load (10 streaming panes, 1 attached, 2 control clients): \
         answered after {} ms, the client redrawn after {} ms (target < 1000 ms, decision 13)",
        answered.as_millis(),
        redrawn.as_millis()
    );

    assert_eq!(world.pid(), pid);
    assert_eq!(world.pane_pids(), panes);
    assert_eq!(attached.pid(), client, "the client re-executed in place");
    wait("the streams to go on", || all_past(&before, 20_000));
    std::fs::write(&stop, "").unwrap();
    let ended = |c: &Control| {
        streaming
            .iter()
            .all(|p| output_of(&c.lines.lock().unwrap(), p).ends_with(b"END\r\n"))
    };
    wait("the end of every stream", || ended(&c1) && ended(&c2));
    for p in &streaming {
        every_line_once(&c1, p, "first");
        every_line_once(&c2, p, "second");
    }
    assert!(!c1.has("%exit") && !c2.has("%exit"));

    let fresh = Attached::new(&world, "s");
    wait("the fresh client's first draw", || {
        fresh.screen().contains(MARK)
    });
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let (a, f) = (attached.screen(), fresh.screen());
        if a == f {
            break;
        }
        assert!(
            Instant::now() < end,
            "re-attached:\n{a}\nfresh attach:\n{f}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}
