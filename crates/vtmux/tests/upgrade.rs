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
        self.command_of(Path::new(BIN), args)
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
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Two sessions, three windows, four `cat -` panes, and state of every
    /// kind the listings show. `server_env` goes to the server's process
    /// environment (the test hooks).
    fn populate(&self, server_env: &[(&str, &str)]) {
        self.populate_from(Path::new(BIN), server_env);
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

    assert_eq!(world.upgrade(&bin()), None);

    assert_eq!(world.pid(), pid);
    assert!(world.server_args().contains("--vtmux-switched-back"));
    assert_eq!(world.pane_pids(), panes);
    assert_eq!(world.observe(), before);
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
        assert_eq!(world.upgrade(&bin()), None, "upgrade {i}");
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
    assert_eq!(world.observe(), before);
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

    assert_eq!(world.upgrade(&bin()), None);

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
    assert_eq!(world.observe(), before);
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

    assert_eq!(world.upgrade(&bin()), None);

    assert_eq!(world.pid(), pid);
    let args = world.server_args();
    assert!(args.contains("--vtmux-switched-back"), "{args}");
    assert_eq!(world.pane_pids(), panes);
    assert_eq!(world.observe(), before);
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
    assert_eq!(world.observe(), before);
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

    assert_eq!(world.upgrade(&wrapper), None);

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

    assert_eq!(world.upgrade(&wrapper), None);

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

fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}
