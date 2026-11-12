//! Live upgrade end to end (live-upgrade spec, *Live upgrade*): a real
//! `target/debug/vtmux` server asked over the frozen `UpgradeRequest` to
//! move onto a binary execs it in place. vtmux only: tmux has no upgrade.
//!
//! Isolation: environment cleared, a private `TMUX_TMPDIR` under /tmp and
//! `HOME`, an explicit `-L`, `-f /dev/null`, pane commands pinned to
//! `cat -`, windows named, `status-right` pinned. Every wait is for state
//! (a listing, a line, bytes on a terminal), never a fixed sleep.
mod common;

use common::{wait, wait_for};

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

    /// [`common::output_within`] of `args`.
    fn run_within(&self, args: &[&str], limit: Duration) -> Option<std::process::Output> {
        common::output_within(&mut self.command(args), limit)
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

    /// The sessions, windows and panes, without what a rebuild without
    /// terminal state loses (history, cursor).
    fn layout(&self) -> String {
        let panes = "#{session_name}:#{window_index}.#{pane_index} #{pane_id} #{pane_pid} \
                     #{pane_tty} #{pane_width}x#{pane_height} #{pane_title} #{pane_dead}";
        let mut out = String::new();
        for args in [
            &[
                "ls",
                "-F",
                "#{session_id} #{session_name} #{session_windows}",
            ][..],
            &[
                "lsw",
                "-a",
                "-F",
                "#{window_id} #{window_name} #{window_layout}",
            ],
            &["lsp", "-a", "-F", panes],
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

    /// The server's `argv[0]`, from `ps` (portable to macOS): the first
    /// word of its command line (the test paths hold no spaces).
    fn server_argv0(&self) -> String {
        self.server_args()
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned()
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

/// An attached client under a PTY; what it was sent, as it comes.
struct Attached {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    bytes: Arc<Mutex<Vec<u8>>>,
    writer: Box<dyn std::io::Write + Send>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Attached {
    fn new(world: &World, session: &str) -> Self {
        Self::of(world, &world.client(), session)
    }

    /// [`Self::new`] run by `binary`.
    fn of(world: &World, binary: &Path, session: &str) -> Self {
        Self::with(world, binary, &["attach", "-t", session])
    }

    /// `binary` with the world's `-L` and `-f`, then `args`, under a PTY.
    fn with(world: &World, binary: &Path, args: &[&str]) -> Self {
        Self::with_env(world, binary, args, &[])
    }

    /// [`Self::with`], `extra` added to the client's environment.
    fn with_env(world: &World, binary: &Path, args: &[&str], extra: &[(&str, &str)]) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(binary);
        cmd.args(["-L", &world.label, "-f", "/dev/null"]);
        cmd.args(args);
        cmd.env_clear();
        for (k, v) in world.env() {
            cmd.env(k, v);
        }
        for (k, v) in extra {
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

    /// The terminal's cursor as an observer of everything it was sent
    /// sees it: shape, blinking and colour.
    fn cursor_style(&self) -> (vtmux_vt::CursorShape, bool, Option<(u8, u8, u8)>) {
        let mut t = vtmux_vt::PaneTerm::new(80, 24, 0);
        t.feed(&self.bytes.lock().unwrap());
        let m = t.screen_modes();
        (m.cursor_shape, m.cursor_blinking, t.cursor_colour())
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

/// `ps`, `pkill -f` and plugins read `argv[0]` as the program the server
/// runs: after an upgrade onto a copy at another path it names the copy,
/// not the binary the server was started from (and `/proc/<pid>/cmdline`
/// says the same on Linux).
#[test]
fn an_upgrade_onto_a_copy_names_the_copy_in_argv0() {
    let world = World::new();
    world.populate(&[]);
    let copy = world.home.path().join("vtmux-copy");
    std::fs::copy(BIN, &copy).unwrap();
    set_executable(&copy);
    let copy = std::fs::canonicalize(&copy).unwrap();
    let pid = world.pid();
    assert_eq!(world.server_argv0(), BIN);

    assert_eq!(world.upgrade(&copy), Some(upgraded()), "the server execed");

    assert_eq!(world.pid(), pid);
    assert!(world.server_args().contains("--vtmux-resume"));
    assert_eq!(world.server_argv0(), copy.to_str().unwrap());
    #[cfg(target_os = "linux")]
    {
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
        let argv0 = cmdline.split(|&b| b == 0).next().unwrap();
        assert_eq!(argv0, copy.as_os_str().as_encoded_bytes());
    }
}

/// A relative `-b` is from the requester's directory, and `argv[0]` shows
/// it cleaned (no `./`, no `..`); a symlink given as `-b` keeps its own
/// path there, as `comm` does, while the server runs its target.
#[test]
fn a_relative_binary_shows_as_a_clean_path_in_argv0() {
    let world = World::new();
    world.populate(&[]);
    let home = std::fs::canonicalize(world.home.path()).unwrap();
    std::fs::create_dir(home.join("sub")).unwrap();
    let copy = home.join("sub").join("vtmux-copy");
    std::fs::copy(BIN, &copy).unwrap();
    set_executable(&copy);
    let link = home.join("vtmux-link");
    std::os::unix::fs::symlink(&copy, &link).unwrap();
    let pid = world.pid();

    for (given, shown) in [
        ("./sub/../sub/./vtmux-copy", &copy),
        ("sub/../vtmux-link", &link),
    ] {
        let out = world
            .run_within(&["vtmux-upgrade", "-b", given], Duration::from_secs(60))
            .expect("vtmux-upgrade answered");
        assert_eq!(out.status.code(), Some(0), "{given}: {out:?}");
        assert_eq!(world.pid(), pid);
        assert_eq!(world.server_argv0(), shown.to_str().unwrap(), "{given}");
        #[cfg(target_os = "linux")]
        {
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
            let argv0 = cmdline.split(|&b| b == 0).next().unwrap();
            assert_eq!(argv0, shown.as_os_str().as_encoded_bytes(), "{given}");
        }
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
    // `ps` still tells which server it is (the oracle runner and `cargo
    // xtask reap` go by `-S`).
    let socket = world.run(&["display", "-p", "#{socket_path}"]);
    let resumed = format!("-S {} --vtmux-resume ", socket.trim_end());
    assert!(
        world.server_args().contains(&resumed),
        "{} lacks {resumed}",
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

/// Input a pane's program has not read yet, which vtmux holds once the
/// PTY takes no more (a 200,000-byte paste into a raw-mode program that
/// reads nothing until told), reaches it after an upgrade, every byte.
/// The program waits for a file, not `tmux wait-for`: a client in flight
/// holds an upgrade.
#[test]
fn input_a_pane_has_not_read_yet_survives_an_upgrade() {
    let world = World::new();
    let home = world.home.path();
    let (go, got, paste) = (home.join("go"), home.join("got"), home.join("paste"));
    let script = format!(
        "stty raw -echo; tmux wait-for -S ready; while [ ! -e '{}' ]; do sleep 1; done; \
         exec head -c 200000 > '{}'",
        go.display(),
        got.display()
    );
    world.run(&[
        "new", "-d", "-s", "a", "-n", "w", "-x", "80", "-y", "24", "sh", "-c", &script,
    ]);
    world.run(&["new", "-d", "-s", "b", "-n", "w", "cat", "-"]);
    world.run(&["wait-for", "ready"]);
    std::fs::write(&paste, vec![b'x'; 200_000]).unwrap();
    world.run(&["load-buffer", paste.to_str().unwrap()]);
    world.run(&["paste-buffer", "-t", "a:w"]);
    assert_eq!(world.upgrade(&bin()), Some(upgraded()), "the server execed");
    std::fs::write(&go, b"").unwrap();
    wait_for(
        "every byte of the paste",
        || std::fs::metadata(&got).map_or(0, |m| m.len()),
        |&n| n == 200_000,
    );
}

/// `-l` outlives a re-attach: after an upgrade, `detach-client -E` runs
/// its command with a login shell (tmux.c:415-416, client.c `client_exec`,
/// `shell_argv0`), as it does before one.
#[test]
fn a_login_client_stays_one_across_an_upgrade() {
    let world = World::new();
    world.populate(&[]);
    let mut attached = Attached::with(&world, &world.client(), &["-l", "attach", "-t", "a"]);
    wait("the client", || world.run(&["lsc"]).lines().count() == 1);
    assert_eq!(world.upgrade(&bin()), Some(upgraded()), "the server execed");
    wait("the re-attach", || {
        attached.args().contains("--vtmux-reattach") && world.run(&["lsc"]).lines().count() == 1
    });
    let out = world.home.path().join("argv0");
    world.run(&[
        "detach-client",
        "-E",
        &format!("echo \"$0\" > {}", out.display()),
    ]);
    wait("the command", || {
        std::fs::read_to_string(&out).is_ok_and(|s| s.ends_with('\n'))
    });
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "-sh\n");
    wait("the client to exit", || attached.exited().is_some());
}

/// D29, live upgrade (spec, *Protocol extensions (D28)*): a pane granted
/// the kitty keyboard protocol keeps it across `vtmux-upgrade` (Ghostty's
/// stack is in the snapshot), with the option turned off since; a pane
/// whose push came while the option was off stays ungranted.
#[test]
fn a_granted_kitty_keys_pane_keeps_its_encoding_across_an_upgrade() {
    let world = World::new();
    world.run(&[
        "new", "-d", "-s", "a", "-n", "w", "-x", "80", "-y", "24", "cat", "-",
    ]);
    let files: Vec<PathBuf> = (0..2)
        .map(|i| world.home.path().join(format!("out{i}")))
        .collect();
    for (i, option) in ["on", "off"].into_iter().enumerate() {
        world.run(&["set", "-s", "vtmux-kitty-keys", option]);
        let script = format!(
            "stty raw -echo; printf '\\033[>1uR{i}'; exec cat > {}",
            files[i].display()
        );
        let target = format!("a:w{i}");
        if i == 0 {
            world.run(&["respawn-pane", "-k", "-t", "a:w", "sh", "-c", &script]);
            world.run(&["rename-window", "-t", "a:w", "w0"]);
        } else {
            world.run(&["neww", "-d", "-t", "a", "-n", "w1", "sh", "-c", &script]);
        }
        wait("the push", || {
            world
                .run(&["capture-pane", "-p", "-t", &target])
                .contains(&format!("R{i}"))
        });
    }
    world.run(&["set", "-s", "vtmux-kitty-keys", "off"]);
    assert_eq!(world.upgrade(&bin()), Some(upgraded()), "the server execed");
    world.run(&["set", "-s", "vtmux-kitty-keys", "on"]);
    for (i, want) in [&b"\x1b[27u.\x1b[99;5u."[..], b"\x1b.\x03."]
        .into_iter()
        .enumerate()
    {
        let target = format!("a:w{i}");
        for key in ["Escape", "C-c"] {
            world.run(&["send-keys", "-t", &target, key]);
            world.run(&["send-keys", "-t", &target, "-H", "2e"]);
        }
        wait(&format!("w{i}'s keys"), || {
            std::fs::read(&files[i]).unwrap_or_default() == want
        });
    }
}

/// Every row a pane shows and holds in history comes through an upgrade
/// cell for cell, with its cursor: a `cat -` pane with line1 to line3
/// typed (each echoed twice), and one with 40 lines of history whose
/// last row is full, its cursor pending a wrap.
#[test]
fn an_upgrade_keeps_every_row_and_the_cursor_of_each_pane() {
    let world = World::new();
    world.populate(&[]);
    for word in ["line1", "line2", "line3"] {
        world.echo("a:w1", word);
    }
    let full_row = "0123456789".repeat(8);
    let script = format!(
        "i=0; while [ $i -lt 40 ]; do i=$((i+1)); echo L$i; done; printf %s {full_row}; exec cat -"
    );
    world.run(&["neww", "-d", "-t", "a", "-n", "w3", "sh", "-c", &script]);
    wait("the history and the full last row", || {
        world
            .run(&["capture-pane", "-p", "-t", "a:w3"])
            .contains(&full_row)
    });
    let shown = |pane: &str| {
        let rows = world.run(&["capture-pane", "-p", "-e", "-S", "-", "-t", pane]);
        let at = world.run(&[
            "display",
            "-p",
            "-t",
            pane,
            "#{history_size} #{cursor_x},#{cursor_y} #{cursor_flag}",
        ]);
        format!("{at}{rows}")
    };
    let before = [shown("a:w1"), shown("a:w3")];
    assert!(
        before[0].contains("line1\nline1\nline2\nline2\nline3\nline3\n"),
        "{}",
        before[0]
    );
    assert!(before[1].starts_with("17 80,23 "), "{}", before[1]);

    assert_eq!(world.upgrade(&bin()), Some(upgraded()), "the server execed");
    assert_eq!([shown("a:w1"), shown("a:w3")], before);
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

/// A key bound to `vtmux-upgrade`, with another typed right after it (the
/// same read): the upgrade is not refused for it, and the second key runs
/// its binding in the image that serves next, once the client re-attached.
/// (Before: `keys pressed meanwhile after vtmux-upgrade would not run`.)
#[test]
fn a_key_typed_right_after_a_bound_upgrade_runs_in_the_new_image() {
    let world = World::new();
    world.populate(&[]);
    let mut attached = Attached::new(&world, "a");
    wait("the client", || world.run(&["lsc"]).lines().count() == 1);
    world.run(&[
        "bind",
        "-n",
        "F5",
        "vtmux-upgrade",
        "-b",
        &bin().display().to_string(),
    ]);
    world.run(&["bind", "-n", "F6", "set", "-g", "@typed", "after"]);
    let pid = world.pid();
    attached.keys("\x1b[15~\x1b[17~");
    wait("the second key's binding", || {
        world
            .command(&["show", "-gv", "@typed"])
            .output()
            .is_ok_and(|o| o.stdout == b"after\n")
    });
    assert_eq!(world.pid(), pid);
    let args = world.server_args();
    assert!(args.contains("--vtmux-resume"), "upgraded: {args}");
    assert!(
        !world.run(&["show-messages"]).contains("would not run"),
        "the upgrade was refused"
    );
}

/// A server with `escape-time` long enough that an ESC typed after a key
/// bound to `vtmux-upgrade` is still held when the server execs, ESC and
/// F6 bound to set options, and the attached client.
fn held_escape_world() -> (World, Attached) {
    let world = World::new();
    world.populate(&[]);
    let attached = Attached::new(&world, "a");
    wait("the client", || world.run(&["lsc"]).lines().count() == 1);
    world.run(&["set", "-s", "escape-time", "1500"]);
    world.run(&[
        "bind",
        "-n",
        "F5",
        "vtmux-upgrade",
        "-b",
        &bin().display().to_string(),
    ]);
    world.run(&["bind", "-n", "F6", "set", "-g", "@typed", "after"]);
    world.run(&["bind", "-n", "Escape", "set", "-g", "@esc", "yes"]);
    (world, attached)
}

fn show_global(world: &World, option: &str) -> Vec<u8> {
    world
        .command(&["show", "-gv", option])
        .output()
        .map(|o| o.stdout)
        .unwrap_or_default()
}

/// A lone ESC typed right after a key bound to `vtmux-upgrade`, still
/// waiting out `escape-time` when the server execs: the image that serves
/// next fires it as Escape. (Before: the re-attach replaced the client's
/// decoder after feeding it the carried ESC, and the key was lost; review
/// of 5e27a857.)
#[test]
fn a_lone_escape_held_across_a_bound_upgrade_fires_in_the_new_image() {
    let (world, mut attached) = held_escape_world();
    let pid = world.pid();
    attached.keys("\x1b[15~\x1b");
    wait("the upgrade", || {
        world.server_args().contains("--vtmux-resume")
    });
    wait("the held Escape's binding", || {
        show_global(&world, "@esc") == b"yes\n"
    });
    assert_eq!(world.pid(), pid);
}

/// F5 (bound to `vtmux-upgrade`), F6 and ESC in one read: F6 goes back to
/// the client's input as a key queued behind the upgrade, the ESC the
/// decoder holds behind it, and the new image runs both. (Before: F6 ran,
/// the ESC was lost at the re-attach; review of 5e27a857.)
#[test]
fn an_escape_held_behind_keys_given_back_fires_in_the_new_image() {
    let (world, mut attached) = held_escape_world();
    let pid = world.pid();
    attached.keys("\x1b[15~\x1b[17~\x1b");
    wait("the second key's binding", || {
        show_global(&world, "@typed") == b"after\n"
    });
    assert!(world.server_args().contains("--vtmux-resume"));
    wait("the held Escape's binding", || {
        show_global(&world, "@esc") == b"yes\n"
    });
    assert_eq!(world.pid(), pid);
}

/// A world whose keys log what they ran: F5 upgrades, Escape, `x`, M-x
/// and F7 (the end mark) each append to `@log`, at the default
/// `escape-time` (10 ms).
fn key_log_world() -> (World, Attached) {
    let world = World::new();
    world.populate(&[]);
    let mut attached = Attached::new(&world, "a");
    wait("the client", || world.run(&["lsc"]).lines().count() == 1);
    // The terminal's answers to the attach's queries: while one is out an
    // ESC waits at least 500 ms (tty-keys.c `tty_keys_next`).
    attached.keys(concat!(
        "\x1b[?62;22c",
        "\x1b[>0;10;1c",
        "\x1bP>|xterm(400)\x1b\\",
        "\x1b[?2026;2$y",
        "\x1b]10;rgb:ffff/ffff/ffff\x1b\\",
        "\x1b]11;rgb:0000/0000/0000\x1b\\",
        "\x1b[8;24;80t",
        "\x1b[4;480;800t",
    ));
    world.run(&["set", "-s", "escape-time", "10"]);
    world.run(&["set", "-g", "@log", ""]);
    world.run(&[
        "bind",
        "-n",
        "F5",
        "vtmux-upgrade",
        "-b",
        &bin().display().to_string(),
    ]);
    for (key, says) in [
        ("F6", "F6,"),
        ("Escape", "ESC,"),
        ("x", "x,"),
        ("M-x", "MX,"),
        ("F7", "end"),
    ] {
        world.run(&["bind", "-n", key, "set", "-ga", "@log", says]);
    }
    (world, attached)
}

/// The log [`key_log_world`] keeps, once F7 has marked its end.
fn key_log(world: &World) -> String {
    let mut log = Vec::new();
    wait("the end mark", || {
        log = show_global(world, "@log");
        log.ends_with(b"end\n")
    });
    String::from_utf8_lossy(&log).trim_end().to_owned()
}

/// Escape, then `x` 300 ms later (more than `escape-time`), typed behind a
/// key bound to `vtmux-upgrade`: they run as Escape and `x`, as they do
/// with no upgrade (F6 first, the control), whether the server still held
/// the ESC at the exec or had decoded it already, its wait out, and given
/// it back while the upgrade waited for a job (`sleep`). (Before: the keys
/// given back were decoded again at one instant, as M-x.) The gap is
/// wide: keys a loaded host's client or server reads within
/// `escape-time` of each other are M-x, rightly (with no gap they are).
#[test]
fn escape_then_a_key_behind_a_bound_upgrade_keep_their_escape_time() {
    for (first, upgrades, job, want) in [
        ("\x1b[17~", false, false, "F6,ESC,x,end"),
        ("\x1b[15~", true, false, "ESC,x,end"),
        ("\x1b[15~", true, true, "ESC,x,end"),
        ("\x1b[15~\x1b[17~", true, true, "F6,ESC,x,end"),
    ] {
        let (world, mut attached) = key_log_world();
        let pid = world.pid();
        if job {
            world.run(&["run-shell", "-b", "sleep 1"]);
        }
        attached.keys(&format!("{first}\x1b"));
        std::thread::sleep(Duration::from_millis(300));
        attached.keys("x");
        if upgrades {
            wait("the upgrade", || {
                world.server_args().contains("--vtmux-resume")
            });
        }
        attached.keys("\x1b[18~");
        assert_eq!(key_log(&world), want, "{first:?}, job {job}");
        assert_eq!(world.pid(), pid);
    }
}

/// A bracketed paste that an upgrade comes in the middle of stays a
/// paste: the keys pasted after it, and an ESC pasted before it and still
/// held, go to the pane as they are, not to their bindings
/// (server-client.c `server_client_is_bracket_paste`). (Before: the
/// re-attach forgot the paste, and `x` or Escape ran its binding.)
#[test]
fn a_paste_an_upgrade_comes_in_the_middle_of_stays_a_paste() {
    for before in ["a", "a\x1b"] {
        let (world, mut attached) = key_log_world();
        attached.keys(&format!("\x1b[200~{before}"));
        wait("the paste's first key", || {
            world
                .run(&["capture-pane", "-p", "-t", "a:w1"])
                .contains('a')
        });
        let out = world.run(&["vtmux-upgrade", "-b", &bin().display().to_string()]);
        assert!(world.server_args().contains("--vtmux-resume"), "{out}");
        attached.keys("x\x1b[201~\x1b[18~");
        assert_eq!(key_log(&world), "end", "{before:?}");
        if before == "a" {
            wait("the pasted keys", || {
                world
                    .run(&["capture-pane", "-p", "-t", "a:w1"])
                    .contains("ax")
            });
        }
    }
}

/// A bracketed paste typed right after a key bound to `vtmux-upgrade`, in
/// the same read and ending in a later one, is carried as the keys are:
/// the upgrade goes through, the paste reaches the pane in the new image
/// as a paste (its `x` runs no binding), and a key after it runs its
/// binding. (Before: refused, `keys pressed meanwhile after vtmux-upgrade
/// would not run`.)
#[test]
fn a_paste_typed_right_after_a_bound_upgrade_is_pasted_in_the_new_image() {
    let (world, mut attached) = key_log_world();
    let pid = world.pid();
    attached.keys("\x1b[15~\x1b[200~pa");
    wait("the upgrade", || {
        world.server_args().contains("--vtmux-resume")
    });
    attached.keys("xste\x1b[201~x\x1b[18~");
    assert_eq!(key_log(&world), "x,end");
    wait("the pasted keys in the pane", || {
        world
            .run(&["capture-pane", "-p", "-t", "a:w1"])
            .contains("paxste")
    });
    assert_eq!(world.pid(), pid);
    assert!(!world.run(&["show-messages"]).contains("would not run"));
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

/// A program that exits 0 on anything is no vtmux to upgrade onto: the
/// dry run passes only when the binary says it read and rebuilt the
/// snapshot (`CHECKED`, with the stream's digest), or, for a vtmux from
/// before that, when its `-V` is exactly a release before it (3.8-beta1,
/// 3.8-beta2). `vtmux-upgrade -b` onto `/bin/echo` (`-V` prints `-V`),
/// `true`, a script that exits 0 (prints nothing), and an old-style vtmux
/// whose `-V` names another version each exits 1 saying so; the server,
/// its panes and their programs carry on. Before the fix each of them
/// replaced the server ("server exited unexpectedly", every pane gone).
/// The old-style vtmux that says `vtmux 3.8-beta2` or `vtmux 3.8-beta1`
/// takes over (a downgrade), and this build takes over from it again.
#[test]
fn a_program_that_exits_0_is_no_vtmux_to_upgrade_onto() {
    let world = World::new();
    world.populate(&[]);
    let pid = world.pid();
    let panes = world.pane_pids();
    let script = world.home.path().join("exits-0");
    std::fs::write(&script, "#!/bin/sh\nexit 0\n\n").unwrap();
    set_executable(&script);
    // The real dry run with its acknowledgement hidden, as a build from
    // before it writes, and `-V` saying `version`.
    let old = |name: &str, version: &str| {
        wrapper(
            &world,
            name,
            &format!(
                "--vtmux-check-snapshot) \"$BIN\" \"$@\" 2>&1 | grep -v '^vtmux-snapshot-checked ' >&2; \
                 exit 0;;\n-V) echo '{version}'; exit 0;;"
            ),
        )
    };
    let candidates = [
        PathBuf::from("/bin/echo"),
        PathBuf::from("/usr/bin/true"),
        script,
        old("vtmux-beta3", "vtmux 3.8-beta3"),
        old("vtmux-as-tmux", "tmux 3.8"),
    ];
    for binary in &candidates {
        let shown = binary.display().to_string();
        let out = world
            .run_within(&["vtmux-upgrade", "-b", &shown], Duration::from_secs(30))
            .expect("vtmux-upgrade answers");
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{shown}: {err}");
        assert!(
            err.contains(&format!("{shown} is not a vtmux that can take over")),
            "{shown}: {err}"
        );
        assert_eq!(world.pid(), pid, "{shown}");
        assert_eq!(world.pane_pids(), panes, "{shown}");
    }
    assert!(!world.server_args().contains("--vtmux-resume"));
    world.echo("a:w1", "still");
    for version in ["vtmux 3.8-beta2", "vtmux 3.8-beta1"] {
        let release = old(&version.replace(' ', "-"), version);
        let out = world
            .run_within(
                &["vtmux-upgrade", "-b", &release.display().to_string()],
                Duration::from_secs(30),
            )
            .expect("vtmux-upgrade answers");
        // 0 only once the new image serves (decision 7).
        assert_eq!(out.status.code(), Some(0), "{version}: {out:?}");
        assert!(world.server_args().contains("--vtmux-resume"), "{version}");
        assert_eq!(world.pid(), pid, "{version}");
        assert_eq!(world.pane_pids(), panes, "{version}");
        assert_eq!(
            world.upgrade(&bin()),
            Some(upgraded()),
            "back from {version}"
        );
    }
    world.echo("a:w1", "after");
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

/// [`a_resume_that_fails_switches_back`] asked for by `vtmux-upgrade` on
/// the command line: not upgraded, so exit 1 (decision 7), the message on
/// stderr; a lossy upgrade exits 2 and a clean one 0.
#[test]
fn a_switched_back_upgrade_exits_1() {
    let world = World::new();
    world.populate(&[("VTMUX_TEST_RESUME_FAIL", "1")]);
    let out = world
        .run_within(&["vtmux-upgrade"], Duration::from_secs(60))
        .expect("vtmux-upgrade answered");
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    assert!(
        err.starts_with(&format!(
            "live upgrade failed: {BIN}: test hook; switched back to "
        )),
        "{err}"
    );
    assert!(world.server_args().contains("--vtmux-switched-back"));
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

/// A copy mode over more history than a snapshot page holds, exported in
/// one page as every vtmux before 8c94c6e did (the test hook, standing in
/// for such an old image): open when the upgrade starts, the dry run
/// refuses it and says to leave copy mode; entered while the dry run runs,
/// it is in the snapshot the exec carries, which the new image's check
/// refuses. The new image then serves the server without that copy mode,
/// every pane's screen and history kept, rather than switch back to the
/// image that wrote it, which can do no better (one before a548110 serves
/// no session at all).
#[test]
fn a_copy_mode_page_past_the_limit_costs_only_that_copy_mode() {
    copy_mode_past_the_limit(&bin(), &[("VTMUX_TEST_COPY_ONE_PAGE", "1")], false, false);
}

/// [`a_copy_mode_page_past_the_limit_costs_only_that_copy_mode`] asked for
/// by `vtmux-upgrade` on the command line (decision 7): the upgrade took
/// effect, short of the copy mode, so it exits 2, its message on stderr;
/// where a failed upgrade (the old image still runs) exits 1 and a clean
/// one 0.
#[test]
fn a_lossy_upgrade_exits_2_where_a_failed_one_exits_1() {
    copy_mode_past_the_limit(&bin(), &[("VTMUX_TEST_COPY_ONE_PAGE", "1")], false, true);
}

/// [`a_copy_mode_page_past_the_limit_costs_only_that_copy_mode`] when the
/// rebuild without that copy mode fails too (the test hook): the new
/// image's own rebuild without any terminal state serves every session,
/// window and pane, before any switch back to the image that wrote the
/// stream, which can't read it either.
#[test]
fn a_copy_mode_page_past_the_limit_keeps_the_sessions_when_nothing_else_rebuilds() {
    copy_mode_past_the_limit(
        &bin(),
        &[
            ("VTMUX_TEST_COPY_ONE_PAGE", "1"),
            ("VTMUX_TEST_RESUME_FAIL", "always"),
        ],
        true,
        false,
    );
}

/// [`a_copy_mode_page_past_the_limit_costs_only_that_copy_mode`] from a
/// real server of a build before 8c94c6e (the reviewer's race: such a
/// server, upgraded onto 8fa8ce1, switched back to itself and served no
/// session). `VTMUX_ONE_PAGE_COPY_BINARY` names it, of this build's
/// protocol version (23ddb70's is).
#[test]
#[ignore = "needs VTMUX_ONE_PAGE_COPY_BINARY, a build before 8c94c6e"]
fn a_server_from_before_8c94c6e_keeps_its_sessions_through_the_copy_mode_race() {
    let old = PathBuf::from(
        std::env::var_os("VTMUX_ONE_PAGE_COPY_BINARY").expect("VTMUX_ONE_PAGE_COPY_BINARY is set"),
    );
    copy_mode_past_the_limit(&old, &[], false, false);
}

/// [`a_copy_mode_page_past_the_limit_keeps_the_sessions_when_nothing_else_rebuilds`]
/// from a real server of a build before 8c94c6e (the reviewer's case: with
/// the test hook, such a server upgraded onto 55500a0 was switched back to
/// and served no session). `VTMUX_ONE_PAGE_COPY_BINARY` as above.
#[test]
#[ignore = "needs VTMUX_ONE_PAGE_COPY_BINARY, a build before 8c94c6e"]
fn a_server_from_before_8c94c6e_keeps_its_sessions_when_nothing_else_rebuilds() {
    let old = PathBuf::from(
        std::env::var_os("VTMUX_ONE_PAGE_COPY_BINARY").expect("VTMUX_ONE_PAGE_COPY_BINARY is set"),
    );
    copy_mode_past_the_limit(&old, &[("VTMUX_TEST_RESUME_FAIL", "always")], true, false);
}

/// The copy mode tests above, on a server started from `server` with
/// `server_env`, upgraded onto this build. With `bare`, the test hook
/// fails every rebuild that keeps terminal state. With `cli`, the upgrade
/// is asked for by the `vtmux-upgrade` command and its exit status and
/// stderr are checked (2, the message), not the frozen handshake's answer.
fn copy_mode_past_the_limit(server: &Path, server_env: &[(&str, &str)], bare: bool, cli: bool) {
    let world = World::new();
    world.populate_from(server, server_env);
    world.run(&["set", "-g", "history-limit", "100000"]);
    world.run(&[
        "neww",
        "-d",
        "-t",
        "a",
        "-n",
        "big",
        "sh",
        "-c",
        "seq 70000; exec cat -",
    ]);
    world.run(&["select-pane", "-t", "a:big", "-T", "t5"]);
    let big = |format: &str| world.run(&["display", "-p", "-t", "a:big", format]);
    wait("the long history", || {
        big("#{history_size} #{pane_current_command}").trim() == "69977 cat"
    });
    let pane = big("#{pane_id}").trim().to_owned();
    // The listing is whole: building every cell of the long history to
    // count its bytes ran past the format time limit (format.c
    // `format_check_time`), cutting ` (active)` off the pane's line.
    let listed = world.run(&["lsp", "-t", "a:big"]);
    assert!(
        listed.trim_end().ends_with(&format!("{pane} (active)")),
        "{listed}"
    );
    let pid = world.pid();
    let panes = world.pane_pids();
    let before = world.observe();
    let layout = world.layout();

    // Open when the upgrade starts: refused, nothing lost.
    world.run(&["copy-mode", "-t", "a:big"]);
    let message = failed(world.upgrade(&bin()));
    assert!(
        message.ends_with(&format!(
            "in {pane}'s copy mode: leave copy mode in {pane} and run vtmux-upgrade again"
        )),
        "{message}"
    );
    assert_eq!(world.observe_like(&before), before);
    world.run(&["send-keys", "-t", "a:big", "-X", "cancel"]);
    wait("copy mode left", || big("#{pane_in_mode}").trim() == "0");

    // Entered while the dry run runs: the dry run passes (it was shown
    // no copy mode), then the wrapper enters it, before the snapshot the
    // exec carries.
    let socket = world.run(&["display", "-p", "#{socket_path}"]);
    let wrapper = world.home.path().join("enters-copy-mode");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nif [ \"$1\" = --vtmux-check-snapshot ]; then\n\
             '{BIN}' \"$@\" || exit $?\n\
             '{BIN}' -S '{}' copy-mode -t a:big </dev/null >/dev/null 2>&1\n\
             exit 0\nfi\nexec '{BIN}' \"$@\"\n",
            socket.trim_end()
        ),
    )
    .unwrap();
    set_executable(&wrapper);
    // The new image serves: the answer says the upgrade took effect, and
    // without what.
    let without = if bare {
        "the panes' terminal state".to_owned()
    } else {
        format!("{pane}'s copy mode")
    };
    let took_effect = format!("live upgrade took effect without {without}: {BIN}: ");
    let message = if cli {
        let out = world
            .run_within(
                &["vtmux-upgrade", "-b", wrapper.to_str().unwrap()],
                Duration::from_secs(60),
            )
            .expect("vtmux-upgrade answered");
        assert_eq!(out.status.code(), Some(2), "{out:?}");
        assert!(out.stdout.is_empty(), "{out:?}");
        String::from_utf8_lossy(&out.stderr).trim_end().to_owned()
    } else {
        failed(world.upgrade(&wrapper))
    };
    assert!(
        message.starts_with(&took_effect)
            && message.contains(&format!("in {pane}'s copy mode"))
            && !message.contains("switch"),
        "{message}"
    );

    assert_eq!(world.pid(), pid);
    let args = world.server_args();
    assert!(
        args.contains("--vtmux-resume") && !args.contains("--vtmux-switched-back"),
        "{args}"
    );
    assert_eq!(world.pane_pids(), panes);
    if bare {
        // Every session, window and pane, their screens and history gone.
        assert_eq!(world.layout(), layout);
    } else {
        // Every pane's history and screen, the long one's too.
        assert_eq!(world.observe_like(&before), before);
    }
    assert_eq!(big("#{pane_in_mode}").trim(), "0");
    world.echo("a:big", "kept");
    let log = world.run(&["show-messages"]);
    assert!(log.contains(&took_effect), "{log}");
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
    // macOS has no fexecve: the script cannot be the switch back's dry run,
    // so the new image, whose own rebuild failed, serves through the last
    // resort and the answer says the upgrade took effect at a second
    // attempt (lu_switchback_last_resort).
    assert!(
        message.starts_with("live upgrade failed: ")
            || (cfg!(target_os = "macos")
                && message.starts_with("live upgrade took effect at a second attempt: ")),
        "{message}"
    );

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
            "#!/bin/sh\nf=$1 n=$2; [ \"$1\" = -S ] && f=$3 n=$4\n\
             if [ \"$f\" = --vtmux-resume ]; then printf damage >>/dev/fd/$n; fi\n\
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
            "#!/bin/sh\nf=$1 n=$2; [ \"$1\" = -S ] && f=$3 n=$4\n\
             if [ \"$f\" = --vtmux-resume ]; then printf damage >>/dev/fd/$n; fi\n\
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

/// An editor (`e` in choose-buffer) open across an upgrade is closed by it
/// (live-upgrade spec, *Open questions*), and its 0600 `/tmp/tmux.XXXXXXXX`
/// file goes with it: the exec runs no destructor, so the old image
/// removes it (spawn.c `spawn_editor_free` removes it when an editor ends).
#[test]
fn an_editor_open_across_an_upgrade_leaves_no_temp_file() {
    use std::os::unix::fs::PermissionsExt as _;
    let world = World::new();
    world.populate(&[]);
    // The editor says which file it was given, then waits.
    let editor = world.home.path().join("ed");
    let given = world.home.path().join("given");
    std::fs::write(
        &editor,
        format!(
            "#!/bin/sh\nprintf %s \"$1\" >{0}.tmp && mv {0}.tmp {0}\nexec sleep 60\n",
            given.display()
        ),
    )
    .unwrap();
    set_executable(&editor);
    world.run(&["set", "-g", "editor", editor.to_str().unwrap()]);
    let mut attached = Attached::new(&world, "a");
    wait("the client", || world.run(&["lsc"]).lines().count() == 1);
    world.run(&["choose-buffer", "-t", "a:w1"]);
    wait("choose-buffer", || {
        world.run(&["display", "-p", "-t", "a:w1", "#{pane_mode}"]) == "buffer-mode\n"
    });
    attached.keys("e");
    wait("the editor to be given its file", || given.exists());
    let file = PathBuf::from(std::fs::read_to_string(&given).unwrap());
    assert!(file.to_str().unwrap().starts_with("/tmp/tmux."), "{file:?}");
    let meta = std::fs::metadata(&file).expect("the editor's file exists while it runs");
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);

    assert_eq!(world.upgrade(&bin()), Some(upgraded()), "the server execed");

    assert!(!file.exists(), "{file:?} outlived the upgrade");
    // The server serves on.
    assert_eq!(world.run(&["has", "-t", "a"]), "");
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

/// A SIGTERM or SIGHUP an attached client catches as it follows an
/// upgrade ends it as an attached tmux client ends for it (client.c
/// `client_signal`): `[terminated]` or `[lost tty]`, status 1, its
/// session left with one client-detached. A test hook raises it in each
/// window that once lost it: in the old image, once the relay has ended
/// and its handlers are gone, just before the exec (the client stayed
/// attached, no hook fired); and in the new image before its `Hello`
/// (`-reattach`: the watcher cut the connection, and the client was given
/// up on).
#[test]
fn a_signal_as_the_client_re_executes_ends_it_as_attached() {
    for (hook, says) in [
        ("TERM", "[terminated]"),
        ("HUP", "[lost tty]"),
        ("TERM-reattach", "[terminated]"),
        ("HUP-reattach", "[lost tty]"),
    ] {
        let world = World::new();
        world.populate(&[]);
        world.run(&["set", "-g", "@log", ""]);
        world.run(&[
            "set-hook",
            "-g",
            "client-detached",
            "set -ga @log '|detached'",
        ]);
        let mut attached = Attached::with_env(
            &world,
            &world.client(),
            &["attach", "-t", "a"],
            &[("VTMUX_TEST_REEXEC_SIGNAL", hook)],
        );
        wait("the client", || world.run(&["lsc"]).lines().count() == 1);
        assert_eq!(world.upgrade(&bin()), Some(upgraded()), "{hook}");
        wait("the client to end", || attached.exited().is_some());
        assert_eq!(attached.exited(), Some(1), "{hook}: {}", attached.since(0));
        wait("its reason", || attached.since(0).contains(says));
        assert!(!attached.since(0).contains("unexpectedly"), "{hook}");
        assert_eq!(world.run(&["lsc"]), "", "{hook}");
        assert_eq!(world.run(&["show", "-gv", "@log"]), "|detached\n", "{hook}");
    }
}

/// The cursor style and colour the terminal was given are not in the
/// snapshot, so the re-attached client's terminal is given them again
/// after it is started again (`tty_start_tty`, tty.c:357-395): a pane's
/// own style and colour, or the reset (`Se`, `Cr`) of a pane with none,
/// whatever the old image's last frame left; a pane's own shows the same
/// before and after.
#[test]
fn a_re_attached_terminal_is_given_its_cursor_style_and_colour_again() {
    use vtmux_vt::CursorShape::{Bar, Default};
    let red = Some((255, 0, 0));
    for (window, command, shown) in [
        (
            "styled",
            r"printf '[5 q]12;red'; exec cat -",
            (Bar, true, red),
        ),
        ("plain", r"exec cat -", (Default, false, None)),
    ] {
        let world = World::new();
        world.populate(&[]);
        world.run(&["neww", "-t", "a", "-n", window, "sh", "-c", command]);
        let attached = Attached::new(&world, "a");
        wait("the cursor the pane asked for", || {
            attached.cursor_style() == shown
        });
        let at = attached.len();
        assert_eq!(world.upgrade(&bin()), Some(upgraded()), "{window}");
        reexecuted(&attached, at);
        // After the terminal started again (`ESC[?996n`): the style and
        // the colour, written whatever the last were.
        let again = "\u{1b}[?996n";
        wait("the cursor written again", || {
            let since = attached.since(at);
            let started = since.rfind(again).map_or("", |i| &since[i + again.len()..]);
            let (style, colour) = match shown.2 {
                Some(_) => ("\u{1b}[5 q", "\u{1b}]12;rgb:ff/00/00\u{7}"),
                None => ("\u{1b}[2 q", "\u{1b}]112\u{7}"),
            };
            started.contains(style) && started.contains(colour)
        });
        // The reset is `Se`, which an observer reads as a steady block: the
        // terminal's own default, so only a pane's own cursor compares.
        if shown.2.is_some() {
            wait("the same cursor", || attached.cursor_style() == shown);
        }
    }
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

/// A binary whose dry run does not say it takes `-S <socket>` (emulated:
/// its check's `TAKES_SOCKET` line dropped, the acknowledgement kept, and
/// `-S` first on its command line fails the start, as a binary's from
/// before `-S` would) gets `--vtmux-resume <fd>` alone, and the server
/// lives on.
#[test]
fn a_binary_that_does_not_take_the_socket_is_not_given_it() {
    let world = World::new();
    world.populate(&[]);
    let pid = world.pid();
    let panes = world.pane_pids();
    let older = wrapper(
        &world,
        "vtmux-older",
        "--vtmux-check-snapshot) \"$BIN\" \"$@\" 2>&1 | grep -v 'takes -S' >&2; exit 0;;\n\
         -S) echo 'usage: vtmux [-2CDlNuVv]' >&2; exit 1;;",
    );
    assert_eq!(world.upgrade(&older), Some(upgraded()));
    assert_eq!(world.pid(), pid);
    assert_eq!(world.pane_pids(), panes);
    let args = world.server_args();
    assert!(
        args.contains("--vtmux-resume") && !args.contains(" -S "),
        "{args}"
    );
    // That image, this build, gives the next one its socket.
    assert_eq!(world.upgrade(&bin()), Some(upgraded()));
    let args = world.server_args();
    assert!(
        args.contains(" -S ") && args.contains("--vtmux-resume"),
        "{args}"
    );
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

/// The finding `low_dry_run_blocks_loop` (`review_upgrade_cmd_2`): the dry
/// run used to hold the server's loop, so while a slow one ran (up to its
/// limit, 3 s here) nothing typed echoed. Now the server serves on: with
/// the new binary's dry run held at its start, what is typed into the
/// attached client reaches its pane and echoes back on the terminal, a
/// command client is answered, and once the dry run goes on the upgrade
/// goes through, carrying what was typed meanwhile (the snapshot the exec
/// gets is taken after the dry run).
#[test]
fn typing_echoes_while_the_dry_run_runs() {
    let world = World::new();
    world.populate(&[]);
    let mut attached = Attached::new(&world, "a");
    wait("the client", || world.run(&["lsc"]).lines().count() == 1);
    world.echo("a:w1", "before");
    wait("the client's draw", || attached.screen().contains("before"));
    let started = world.home.path().join("started");
    let release = world.home.path().join("release");
    // Held at most 20 s, so a failing run leaves no dry run behind.
    let binary = wrapper(
        &world,
        "vtmux-slow",
        &format!(
            "--vtmux-check-snapshot) : >'{}'; i=0; while [ ! -e '{}' ] && [ $i -lt 400 ]; \
             do sleep 0.05; i=$((i+1)); done;;",
            started.display(),
            release.display()
        ),
    );
    let path = world.run(&["display", "-p", "#{socket_path}"]);
    *world.socket.lock().unwrap() = Some(PathBuf::from(path.trim_end()));
    let pid = world.pid();
    let panes = world.pane_pids();
    thread::scope(|scope| {
        let asked = scope.spawn(|| world.upgrade_at(&binary));
        wait("the dry run to start", || started.exists());
        let at = attached.len();
        attached.keys("during\r");
        wait("the echo on the client", || {
            attached.since(at).contains("during")
        });
        assert_eq!(world.run(&["display", "-p", "served"]), "served\n");
        assert!(!asked.is_finished(), "the dry run is still running");
        std::fs::write(&release, "").unwrap();
        assert_eq!(asked.join().unwrap(), Some(upgraded()));
    });
    assert_eq!(world.pid(), pid);
    assert_eq!(world.pane_pids(), panes);
    assert!(world.server_args().contains("--vtmux-resume"));
    let shown = world.run(&["capture-pane", "-p", "-t", "a:w1"]);
    assert_eq!(shown.matches("during").count(), 2, "{shown}");
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
                ..Default::default()
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

/// A protocol bump end to end, against `VTMUX_PREVIOUS_BINARY`, a build of
/// the protocol before this one's (`git worktree add` the commit before
/// the bump and `cargo build -p vtmux` there). Clients of either version
/// on a server of the other get the version mismatch at once, never a
/// connection left waiting, and a newer one the upgrade hint; this
/// build's `vtmux-upgrade` moves the older server onto it, the older
/// attached client re-executes this build and comes back, and the older
/// control client reads on; an older control client is served after it.
#[test]
#[ignore = "needs VTMUX_PREVIOUS_BINARY, a build of the previous protocol version"]
fn a_server_of_the_previous_protocol_moves_onto_this_build() {
    let previous = PathBuf::from(
        std::env::var_os("VTMUX_PREVIOUS_BINARY").expect("VTMUX_PREVIOUS_BINARY is set"),
    );
    let (new, old) = (protocol::PROTOCOL_VERSION, protocol::PROTOCOL_VERSION - 1);
    let hint = "run 'tmux vtmux-upgrade' to move the server onto this binary";
    let world = World::new();
    *world.client.lock().unwrap() = previous.clone();
    world.populate(&[]);
    let mut attached = Attached::new(&world, "a");
    let mut control = Control::new(&world, "b");
    wait("both clients", || world.run(&["lsc"]).lines().count() == 2);
    let refused = |binary: &Path, session_client: bool, client: u32, server: u32| {
        let line = format!("protocol version mismatch (client {client}, server {server})");
        if session_client {
            let mut a = Attached::of(&world, binary, "a");
            wait("the refused client to exit", || a.exited().is_some());
            assert_eq!(a.exited(), Some(1));
            let text = a.since(0);
            assert!(text.contains(&line), "{text:?}");
            assert_eq!(text.contains(hint), client > server, "{text:?}");
        } else {
            let out = world.command_of(binary, &["ls"]).output().unwrap();
            let err = String::from_utf8_lossy(&out.stderr);
            assert_eq!(out.status.code(), Some(1), "{err}");
            assert!(err.starts_with(&line), "{err}");
            assert_eq!(err.contains(hint), client > server, "{err}");
        }
    };
    refused(&bin(), true, new, old);
    refused(&bin(), false, new, old);
    let pid = world.pid();
    let panes = world.pane_pids();

    let at = attached.len();
    let out = world
        .command_of(&bin(), &["vtmux-upgrade"])
        .output()
        .unwrap();
    assert_eq!(
        (out.status.code(), String::from_utf8_lossy(&out.stderr)),
        (Some(0), "".into())
    );
    *world.client.lock().unwrap() = bin();
    assert_eq!(world.pid(), pid);
    assert_eq!(world.pane_pids(), panes);
    reexecuted(&attached, at);
    #[cfg(target_os = "linux")]
    assert_eq!(
        std::fs::read_link(format!("/proc/{}/exe", attached.pid())).unwrap(),
        std::fs::canonicalize(BIN).unwrap()
    );
    attached.keys("typed\r");
    wait("the keys to reach the pane", || {
        world
            .run(&["capture-pane", "-p", "-t", "a:w1"])
            .contains("typed")
    });
    control.send("display -p 'after #{session_name}'");
    wait("the older control client reading on", || {
        control.has("after b")
    });
    assert_eq!(world.run(&["lsc"]).lines().count(), 2);

    refused(&previous, true, old, new);
    refused(&previous, false, old, new);
    *world.client.lock().unwrap() = previous.clone();
    let mut later = Control::new(&world, "b");
    later.send("display -p 'later #{session_name}'");
    wait("a later older control client", || later.has("later b"));
    *world.client.lock().unwrap() = bin();
    assert!(attached.exited().is_none());
}

/// A downgrade under an editor up over a long history, onto
/// `VTMUX_PRE_0038_BINARY` (a build from before Ghostty patch 0038, which
/// copied the history on `?1049h`: `git worktree add` main and `cargo
/// build -p vtmux` there), and back. The build before shows the copy this
/// build writes beside the primary screen (`AltEntry::history`), so its
/// `history_size` and capture stay tmux's, and it goes on applying a
/// history-limit lowered under the alternate screen (options.c
/// `options_push_changes`, session.c `session_update_history`). The numbers
/// are the reference tmux 3.8's for the same input at 80x24.
#[test]
#[ignore = "needs VTMUX_PRE_0038_BINARY, a build from before Ghostty patch 0038"]
fn a_downgrade_under_an_editor_keeps_the_history_it_hides() {
    let before = PathBuf::from(
        std::env::var_os("VTMUX_PRE_0038_BINARY").expect("VTMUX_PRE_0038_BINARY is set"),
    );
    let world = World::new();
    world.populate(&[]);
    world.run(&["set", "-g", "history-limit", "400000"]);
    let program = "seq 300000; printf '\\033[?1049hvim'; read x; \
                   printf '\\033[?1049l'; seq 50; exec cat -";
    world.run(&["neww", "-d", "-t", "a", "-n", "big", "sh", "-c", program]);
    world.run(&["select-pane", "-t", "a:big", "-T", "t5"]);
    let big = |format: &str| {
        world
            .run(&["display", "-p", "-t", "a:big", format])
            .trim()
            .to_owned()
    };
    // The history's first and last lines and its length.
    let history = || {
        let text = world.run(&["capture-pane", "-p", "-t", "a:big", "-S", "-", "-E", "-1"]);
        let lines: Vec<&str> = text.lines().collect();
        format!(
            "{} {} {}",
            lines.first().unwrap_or(&""),
            lines.last().unwrap_or(&""),
            lines.len()
        )
    };
    wait("the editor up over the long history", || {
        big("#{history_size} #{alternate_on}") == "299977 1"
    });
    world.run(&["set", "-g", "history-limit", "199999"]);
    assert_eq!(big("#{history_size}"), "199999");
    assert_eq!(history(), "99979 299977 199999");
    let pid = world.pid();

    assert_eq!(
        world.upgrade(&before),
        Some(upgraded()),
        "onto the build before"
    );
    assert_eq!(world.pid(), pid);
    assert_eq!(big("#{history_size} #{alternate_on}"), "199999 1");
    assert_eq!(history(), "99979 299977 199999");
    world.run(&["set", "-g", "history-limit", "99999"]);
    assert_eq!(big("#{history_size}"), "99999");
    assert_eq!(history(), "199979 299977 99999");
    world.run(&["send-keys", "-t", "a:big", "go", "Enter"]);
    wait("the editor gone and the lines out", || {
        big("#{alternate_on} #{pane_current_command}") == "0 cat"
    });
    assert_eq!(big("#{history_size}"), "90050");
    assert_eq!(history(), "209978 27 90050");

    assert_eq!(world.upgrade(&bin()), Some(upgraded()), "and back");
    assert_eq!(world.pid(), pid);
    assert_eq!(big("#{history_size} #{alternate_on}"), "90050 0");
    assert_eq!(history(), "209978 27 90050");
}

/// Live upgrade both ways with 3.8-beta3, the last release with
/// `vtmux-persist` (live-upgrade spec, *Removal*), against
/// `VTMUX_BETA3_BINARY` (the release binary, or `cargo build -p vtmux` at
/// its commit, 034e438a). A beta3 server restored from its checkpoint (a
/// pane still waiting, dead with its saved screen, and one respawned
/// under beta3's restart guard), with `vtmux-persist` on, a live pane with
/// history and a beta3 client attached: onto this build, back onto beta3
/// and onto this build again. Each time the pid, the panes, their screens
/// and history stay and the client follows. This build takes the waiting
/// pane as an ordinary dead pane, knows no persistence option (`@` ones
/// kept), lets the guarded pane's program exit as any pane's and writes
/// nothing to the checkpoint directory.
#[test]
#[ignore = "needs VTMUX_BETA3_BINARY, a 3.8-beta3 vtmux"]
#[expect(clippy::too_many_lines, reason = "three upgrades in a row")]
fn a_beta3_server_upgrades_onto_this_build_and_back() {
    let beta3 =
        PathBuf::from(std::env::var_os("VTMUX_BETA3_BINARY").expect("VTMUX_BETA3_BINARY is set"));
    let world = World::new();
    *world.client.lock().unwrap() = beta3.clone();
    let shown = |pane: &str, word: &str| {
        world
            .run(&["capture-pane", "-p", "-t", pane])
            .contains(word)
    };
    // A beta3 checkpoint: two panes that printed a word each.
    let out = output_retrying(|| {
        world.command_of(
            &beta3,
            &[
                "new",
                "-d",
                "-s",
                "r",
                "-x",
                "80",
                "-y",
                "24",
                "sh",
                "-c",
                "echo saved-one; exec cat -",
            ],
        )
    });
    assert!(out.status.success(), "{out:?}");
    world.run(&[
        "splitw",
        "-d",
        "-t",
        "r",
        "sh",
        "-c",
        "echo saved-two; exec cat -",
    ]);
    wait("both words", || {
        shown("r:0.0", "saved-one") && shown("r:0.1", "saved-two")
    });
    world.run(&["set", "-s", "vtmux-persist", "full"]);
    world.run(&["set", "-g", "vtmux-persist-drop", "@murmur_*"]);
    world.run(&["kill-server"]);
    let state = world.home.path().join(".local/state/vtmux");
    let manifest = || {
        std::fs::read_dir(&state)
            .ok()?
            .filter_map(Result::ok)
            .map(|e| e.path().join("manifest"))
            .find(|m| m.exists())
    };
    wait("beta3's checkpoint", || manifest().is_some());
    // The next start restores it; then sessions of every kind, a pane
    // with history, a user option, a beta3 client.
    world.populate(&[]);
    world.run(&["set", "-g", "@keep", "yes"]);
    world.run(&[
        "neww",
        "-d",
        "-t",
        "b",
        "-n",
        "hist",
        "sh",
        "-c",
        "seq 3000; exec cat -",
    ]);
    wait("the history", || shown("b:hist", "3000"));
    let dead = |pane: &str| world.run(&["display", "-p", "-t", pane, "#{pane_dead}"]);
    assert_eq!(dead("r:0.0"), "1\n", "beta3 restored it");
    assert!(shown("r:0.0", "saved-one"));
    // Respawned with no input yet: beta3 guards it (`restart_from`).
    world.run(&["respawn-pane", "-t", "r:0.1", "cat", "-"]);
    let guarded = world
        .run(&["display", "-p", "-t", "r:0.1", "#{pane_pid}"])
        .trim()
        .to_owned();
    let mut attached = Attached::of(&world, &beta3, "a");
    wait("the client", || world.run(&["lsc"]).lines().count() == 1);
    let pid = world.pid();
    let state_now = || {
        let mut out = world.run(&[
            "lsp",
            "-a",
            "-F",
            "#{pane_id} #{pane_pid} #{pane_dead} #{pane_dead_status} #{history_size}",
        ]);
        for pane in ["r:0.0", "b:hist"] {
            out.push_str(&world.run(&["capture-pane", "-p", "-S", "-", "-t", pane]));
        }
        out
    };
    let before = state_now();

    // Onto this build.
    let at = attached.len();
    assert_eq!(world.upgrade(&bin()), Some(upgraded()), "onto this build");
    *world.client.lock().unwrap() = bin();
    assert_eq!(world.pid(), pid);
    assert_eq!(state_now(), before);
    reexecuted(&attached, at);
    assert_eq!(dead("r:0.0"), "1\n");
    let out = world
        .command(&["show", "-s", "vtmux-persist"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("invalid option: vtmux-persist"),
        "{out:?}"
    );
    assert!(!world.run(&["show", "-g"]).contains("vtmux-persist"));
    assert_eq!(world.run(&["show", "-gv", "@keep"]), "yes\n");
    let written = std::fs::read(manifest().unwrap()).unwrap();
    // The guarded program's exit is any pane's: `remain-on-exit` is off,
    // so the pane goes (beta3 kept it as restored).
    let raw = rustix::process::Pid::from_raw(guarded.parse().unwrap()).unwrap();
    rustix::process::kill_process(raw, rustix::process::Signal::TERM).unwrap();
    wait("the guarded pane closed", || {
        world.run(&["lsp", "-t", "r", "-F", "#{pane_index}"]) == "0\n"
    });
    let before = state_now();

    // Back onto beta3: it reads no persistence option, so it writes none.
    let at = attached.len();
    assert_eq!(world.upgrade(&beta3), Some(upgraded()), "onto beta3");
    *world.client.lock().unwrap() = beta3.clone();
    assert_eq!(world.pid(), pid);
    assert_eq!(state_now(), before);
    reexecuted(&attached, at);
    assert_eq!(world.run(&["show", "-sv", "vtmux-persist"]), "off\n");
    assert_eq!(world.run(&["show", "-gv", "@keep"]), "yes\n");

    // And onto this build again.
    let at = attached.len();
    assert_eq!(
        world.upgrade(&bin()),
        Some(upgraded()),
        "onto this build again"
    );
    *world.client.lock().unwrap() = bin();
    assert_eq!(world.pid(), pid);
    assert_eq!(state_now(), before);
    reexecuted(&attached, at);
    attached.keys("typed\r");
    wait("the keys to reach the pane", || shown("a:w1", "typed"));
    world.run(&["kill-server"]);
    assert_eq!(
        std::fs::read(manifest().unwrap()).unwrap(),
        written,
        "the checkpoint as beta3 left it"
    );
}
