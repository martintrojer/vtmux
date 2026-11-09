//! Runs one scenario against one tmux-compatible binary, in isolation.
//!
//! Isolation (spec, *Behaviour oracle*; tmux.c:557-571 lets an inherited
//! `$TMUX` beat `TMUX_TMPDIR`): the environment is cleared, `HOME` is a private
//! temp dir, and every invocation gets `-S <root>/s -f /dev/null`. The root is
//! always a short `/tmp/vto-<6 hex>` (never `$TMPDIR`): tmux refuses socket
//! paths of `sizeof sun_path` or more (client.c:114-116; 104 on macOS).

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use vtmux_vt::{CaptureOpts, PaneTerm, capture};

use crate::reap;

use super::normalize::Normalizer;
use super::scenario::{
    Divergence, Observation, Scenario, Settle, Setup, Step, WaitPane, WaitScreen,
};

/// An observation plus where it came from.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct Item {
    /// Index into the scenario's `steps`.
    pub step: usize,
    pub observation: Observation,
    /// Set from a preceding `divergence` step; never stored in traces.
    #[serde(skip)]
    pub divergence: Option<Divergence>,
}

// The limits below only end a hang. A host at a load of a hundred starves a
// process for tens of seconds (a fresh `tmux` client, a server start, an
// attach's first draw), and a limit that runs out turns that into a
// `timeout` observation and a cascade of failures behind it.
const CLI_TIMEOUT: Duration = Duration::from_secs(120);
const WAIT_TIMEOUT: Duration = Duration::from_secs(60);
/// After a CLI process exits, how long its output pipes may stay open.
const DRAIN_LIMIT: Duration = Duration::from_secs(30);
const WAIT_SCREEN_LIMIT: Duration = Duration::from_secs(60);
const DEFAULT_QUIET: Duration = Duration::from_millis(150);
const SETTLE_LIMIT: Duration = Duration::from_secs(30);
/// How long `settle` waits for the first output a just-sent input provokes.
const REACTION_LIMIT: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(5);
/// An upgrade waits up to 3 s for jobs and commands in flight, then runs a
/// dry run of at most 3 s (live-upgrade spec, *Sequence*).
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(30);
/// The server gives a client 10 s to re-attach after its notice.
const REATTACH_LIMIT: Duration = Duration::from_secs(15);
/// What a re-attached client is sent first (`Caps::restart`, as after
/// `MSG_WAKEUP`): its terminal started again, ahead of the full redraw.
const REATTACHED: &[u8] = b"\x1b[?996n";
/// How the server refuses an upgrade under its strict test hook
/// (`VTMUX_TEST_UPGRADE_STRICT`, vtmux-server `server/upgrade.rs`
/// `STRICT_REFUSAL`): something in flight, or open that the upgrade would
/// close by design. Not a failure: the inserted upgrade comes a step later.
const STRICT_REFUSAL: &str = "test hook: not now: ";

/// Upgrades `--upgrade-every` inserted, and the times one was put off to
/// a later step, across every run of this process.
pub static UPGRADES: AtomicUsize = AtomicUsize::new(0);
pub static DEFERRED: AtomicUsize = AtomicUsize::new(0);
const HISTORY_LIMIT: usize = 2000;

/// Where run roots live. Fixed, so a long `$TMPDIR` cannot lengthen socket paths.
const ROOT_BASE: &str = "/tmp";
/// Socket paths must stay under this (the strictest `sun_path` is 104 bytes).
const MAX_SOCKET_LEN: usize = 100;

static RUN_SEQ: AtomicUsize = AtomicUsize::new(0);

/// Six hex digits that differ per run and per process.
fn nonce() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_usize(RUN_SEQ.fetch_add(1, Ordering::SeqCst));
    h.write_u32(std::process::id());
    format!("{:06x}", h.finish() & 0xff_ffff)
}

/// The socket inside `root`, checked against `MAX_SOCKET_LEN`.
fn socket_path(root: &Path) -> Result<PathBuf> {
    let socket = root.join("s");
    if socket.as_os_str().len() >= MAX_SOCKET_LEN {
        return Err(format!(
            "socket path {} is {} bytes, limit {MAX_SOCKET_LEN}",
            socket.display(),
            socket.as_os_str().len()
        ));
    }
    Ok(socket)
}

/// Create a fresh private run root `ROOT_BASE/vto-<6 hex>`.
fn make_root() -> Result<PathBuf> {
    for _ in 0..100 {
        let root = Path::new(ROOT_BASE).join(format!("vto-{}", nonce()));
        let mut b = fs::DirBuilder::new();
        b.mode(0o700);
        match b.create(&root) {
            Ok(()) => return Ok(root),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("{}: {e}", root.display())),
        }
    }
    Err(format!("no free run root under {ROOT_BASE}"))
}

type Result<T> = std::result::Result<T, String>;

/// Output activity across every client of a run.
struct Activity {
    last: Mutex<Instant>,
    total: AtomicUsize,
}

impl Activity {
    fn bump(&self, n: usize) {
        *self.last.lock().unwrap() = Instant::now();
        self.total.fetch_add(n, Ordering::SeqCst);
    }
}

fn spawn_reader(
    mut r: impl Read + Send + 'static,
    act: Arc<Activity>,
    done: Arc<AtomicBool>,
    mut sink: impl FnMut(&[u8]) + Send + 'static,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    sink(&buf[..n]);
                    act.bump(n);
                }
            }
        }
        done.store(true, Ordering::SeqCst);
    });
}

struct Control {
    child: Child,
    stdin: Option<ChildStdin>,
    out: Arc<Mutex<Vec<u8>>>,
    err: Arc<Mutex<Vec<u8>>>,
    done: Arc<AtomicBool>,
    /// Stream offset when the last line was sent.
    mark: usize,
}

enum Event {
    Bytes(Vec<u8>),
    Resize(u16, u16),
}

struct Attach {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    events: Arc<Mutex<Vec<Event>>>,
}

/// What a run does beyond the scenario's own steps.
#[derive(Debug, Clone, Copy, Default)]
pub struct Opts {
    /// The binary under test is vtmux: `upgrade` steps run (the reference
    /// tmux has no upgrade and skips them).
    pub vtmux: bool,
    /// Also upgrade the server onto itself after every Nth step (`replay
    /// --upgrade-every N`), never observed: a scenario must pass as it
    /// does without (live-upgrade spec, *Tests*).
    pub upgrade_every: Option<usize>,
}

pub struct Runner<'a> {
    bin: &'a Path,
    sc: &'a Scenario,
    opts: Opts,
    root: PathBuf,
    socket: PathBuf,
    norm: Normalizer,
    act: Arc<Activity>,
    controls: HashMap<String, Control>,
    /// The attached clients by id; the default one's is empty.
    attaches: HashMap<String, Attach>,
    size: [u16; 2],
    items: Vec<Item>,
    pending_div: Option<Divergence>,
    /// Output total when an input was sent that should provoke output.
    expect_after: Option<usize>,
    /// Output total when the last settle returned: what `settle { react }`
    /// waits to exceed after a CLI step.
    baseline: usize,
}

impl Drop for Runner<'_> {
    fn drop(&mut self) {
        // Best effort: stop the server this run started, then every client.
        let _ = self.cli_raw(&["kill-server".into()], Duration::from_secs(5));
        for c in self.controls.values_mut() {
            let _ = c.child.kill();
            let _ = c.child.wait();
        }
        for a in self.attaches.values_mut() {
            let _ = a.child.kill();
            let _ = a.child.wait();
        }
        // A wedged or unreachable server survives kill-server (it daemonized):
        // kill whatever still holds this run's socket.
        let socket = self.socket.to_string_lossy().into_owned();
        reap::kill_matching(|ps| socket_pids(ps, &socket, std::process::id()));
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Pids of processes started with `-S <socket>` (the run root is unique).
fn socket_pids(ps: &str, socket: &str, own: u32) -> Vec<u32> {
    let needle = format!("-S {socket} ");
    reap::pids_where(ps, own, |args| format!("{args} ").contains(&needle))
}

/// Run `sc` against `bin` and return its normalised observations.
pub fn run(bin: &Path, sc: &Scenario, opts: Opts) -> Result<Vec<Item>> {
    let mut r = Runner::new(bin, sc, opts)?;
    r.run_steps()?;
    Ok(std::mem::take(&mut r.items))
}

impl<'a> Runner<'a> {
    fn new(bin: &'a Path, sc: &'a Scenario, opts: Opts) -> Result<Self> {
        let root = make_root()?;
        let made = Self::init_root(bin, &root);
        let socket = match made.and_then(|()| socket_path(&root)) {
            Ok(s) => s,
            Err(e) => {
                let _ = fs::remove_dir_all(&root);
                return Err(e);
            }
        };
        let norm = Normalizer::new(&root.to_string_lossy(), &socket.to_string_lossy());
        Ok(Self {
            bin,
            sc,
            opts,
            root,
            socket,
            norm,
            act: Arc::new(Activity {
                last: Mutex::new(Instant::now()),
                total: AtomicUsize::new(0),
            }),
            controls: HashMap::new(),
            attaches: HashMap::new(),
            size: sc.size,
            items: Vec::new(),
            pending_div: None,
            expect_after: None,
            baseline: 0,
        })
    }

    fn init_root(bin: &Path, root: &Path) -> Result<()> {
        fs::create_dir(root.join("home")).map_err(|e| format!("{}: {e}", root.display()))?;
        // Pane programs signal with `tmux wait-for -S`; $TMUX gives them the socket.
        fs::create_dir(root.join("bin")).map_err(|e| e.to_string())?;
        std::os::unix::fs::symlink(bin, root.join("bin/tmux")).map_err(|e| e.to_string())
    }

    fn env(&self) -> Vec<(&'static str, String)> {
        let mut env = vec![
            (
                "HOME",
                self.root.join("home").to_string_lossy().into_owned(),
            ),
            ("SHELL", "/bin/sh".into()),
            ("PS1", "$ ".into()),
            ("TERM", "xterm-256color".into()),
            ("LC_ALL", "C.UTF-8".into()),
            (
                "PATH",
                format!(
                    "{}:/usr/local/bin:/usr/bin:/bin",
                    self.root.join("bin").display()
                ),
            ),
            ("TMUX_TMPDIR", self.root.to_string_lossy().into_owned()),
        ];
        if self.opts.vtmux && self.opts.upgrade_every.is_some() {
            env.push(("VTMUX_TEST_UPGRADE_STRICT", "1".into()));
        }
        env
    }

    fn base_args(&self) -> Vec<String> {
        vec![
            "-S".into(),
            self.socket.to_string_lossy().into_owned(),
            "-f".into(),
            "/dev/null".into(),
        ]
    }

    fn command(&self, argv: &[String]) -> Command {
        let mut c = Command::new(self.bin);
        c.env_clear()
            .envs(self.env())
            .args(self.base_args())
            .args(argv);
        c.current_dir(self.root.join("home"));
        c
    }

    fn push(&mut self, step: usize, observation: Observation) {
        let divergence = self.pending_div.take();
        self.items.push(Item {
            step,
            observation,
            divergence,
        });
    }

    fn cli_raw(&self, argv: &[String], timeout: Duration) -> Result<Option<(String, String, i32)>> {
        let mut cmd = self.command(argv);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("{}: {e}", self.bin.display()))?;
        let out = Arc::new(Mutex::new(Vec::new()));
        let err = Arc::new(Mutex::new(Vec::new()));
        let (o, e) = (Arc::clone(&out), Arc::clone(&err));
        let (od, ed) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let quiet = Arc::new(Activity {
            last: Mutex::new(Instant::now()),
            total: AtomicUsize::new(0),
        });
        spawn_reader(
            child.stdout.take().unwrap(),
            Arc::clone(&quiet),
            Arc::clone(&od),
            move |b| o.lock().unwrap().extend_from_slice(b),
        );
        spawn_reader(
            child.stderr.take().unwrap(),
            Arc::clone(&quiet),
            Arc::clone(&ed),
            move |b| e.lock().unwrap().extend_from_slice(b),
        );
        let start = Instant::now();
        let status = loop {
            if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
                break s;
            }
            if start.elapsed() > timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(None);
            }
            thread::sleep(POLL);
        };
        // The pipes close with the process unless a daemon inherited them. The
        // readers are threads on a host that may starve them for seconds:
        // stopping early would cut the output short.
        let limit = Instant::now() + DRAIN_LIMIT;
        while !(od.load(Ordering::SeqCst) && ed.load(Ordering::SeqCst)) && Instant::now() < limit {
            thread::sleep(POLL);
        }
        let text =
            |b: &Arc<Mutex<Vec<u8>>>| String::from_utf8_lossy(&b.lock().unwrap()).into_owned();
        Ok(Some((text(&out), text(&err), status.code().unwrap_or(-1))))
    }

    fn cli(&mut self, step: usize, argv: &[String], what: &str) -> Result<()> {
        let obs = match self.cli_raw(argv, CLI_TIMEOUT)? {
            Some((stdout, stderr, exit)) => Observation::Cli {
                stdout: self.norm.text(&stdout),
                stderr: self.norm.text(&stderr),
                exit,
            },
            None => Observation::Timeout {
                what: what.to_string(),
            },
        };
        self.push(step, obs);
        Ok(())
    }

    fn run_steps(&mut self) -> Result<()> {
        let sc = self.sc;
        let mut upgrade_due = false;
        for (i, step) in sc.steps.iter().enumerate() {
            match step {
                Step::Run(argv) => self.cli(i, argv, &format!("run {argv:?}"))?,
                Step::Setup(Setup::StaleSocket) => self.stale_socket()?,
                Step::Control(c) => self.control_start(&c.id, &c.argv)?,
                Step::ControlSend(c) => {
                    let ctl = self.control_mut(&c.id)?;
                    ctl.mark = ctl.out.lock().unwrap().len();
                    if let Some(w) = ctl.stdin.as_mut() {
                        // EPIPE is fine: the client may already have exited.
                        let _ = w.write_all(format!("{}\n", c.line).as_bytes());
                        let _ = w.flush();
                    }
                    self.expect_after = Some(self.act.total.load(Ordering::SeqCst));
                }
                Step::ControlWait(c) => {
                    if self.control_wait(&c.id, &c.until)? {
                        self.settle(DEFAULT_QUIET);
                    } else {
                        self.push(
                            i,
                            Observation::Timeout {
                                what: format!("control_wait {} {}", c.id, c.until),
                            },
                        );
                    }
                }
                Step::ControlClose(c) => self.control_close(i, &c.id)?,
                Step::Attach(a) => {
                    self.attach_start(
                        &a.session,
                        a.id.clone().unwrap_or_default(),
                        a.term.as_deref(),
                    )?;
                }
                Step::Keys(k) => self.keys("", k)?,
                Step::KeysTo(k) => self.keys(&k.id, &k.keys)?,
                Step::Feed(f) => {
                    let argv: Vec<String> = ["send-keys", "-t", &f.pane, "-l", &f.bytes]
                        .map(String::from)
                        .to_vec();
                    match self.cli_raw(&argv, CLI_TIMEOUT)? {
                        Some((_, _, 0)) => {}
                        _ => self.cli(i, &argv, "feed")?,
                    }
                    self.expect_after = Some(self.act.total.load(Ordering::SeqCst));
                }
                Step::Resize([c, r]) => {
                    let total = self.act.total.load(Ordering::SeqCst);
                    let a = self
                        .attaches
                        .get_mut("")
                        .ok_or("resize: no attached client")?;
                    a.events.lock().unwrap().push(Event::Resize(*c, *r));
                    a.master
                        .resize(PtySize {
                            rows: *r,
                            cols: *c,
                            pixel_width: 0,
                            pixel_height: 0,
                        })
                        .map_err(|e| e.to_string())?;
                    self.size = [*c, *r];
                    self.expect_after = Some(total);
                }
                Step::Settle(s) => self.settle_step(i, s),
                Step::WaitFor(chan) => {
                    let argv = vec!["wait-for".to_string(), chan.clone()];
                    self.cli(i, &argv, &format!("wait_for {chan}"))?;
                }
                Step::WaitScreen(w) => self.wait_screen_step(i, w)?,
                Step::WaitPane(w) => self.wait_pane(i, w)?,
                Step::Screen(s) => {
                    let id = s.id.clone().unwrap_or_default();
                    self.observe_client(i, &id, ClientView::Screen)?;
                }
                Step::Title(_) => self.observe_client(i, "", ClientView::Title)?,
                Step::Progress(s) => {
                    let id = s.id.clone().unwrap_or_default();
                    self.observe_client(i, &id, ClientView::Progress)?;
                }
                Step::Divergence(d) => self.pending_div = Some(d.clone()),
                Step::Upgrade(_) if self.opts.vtmux => {
                    self.upgrade(i, false);
                }
                Step::Upgrade(_) => {}
            }
            self.upgrade_after(i, &mut upgrade_due);
        }
        if self.pending_div.is_some() {
            return Err("divergence step is not followed by an observation".into());
        }
        Ok(())
    }

    /// `--upgrade-every`: once `n` steps have run since the last inserted
    /// upgrade, the next one after step `i`, though not between an input
    /// and the step that waits for its reaction (the redraw would pass for
    /// the reaction), and not after the last step (nothing would observe
    /// it). One the server puts off (its strict hook) comes after the next
    /// step that allows one.
    fn upgrade_after(&mut self, i: usize, due: &mut bool) {
        if self.opts.vtmux
            && self
                .opts
                .upgrade_every
                .is_some_and(|n| (i + 1).is_multiple_of(n))
        {
            *due = true;
        }
        let next = self.sc.steps.get(i + 1);
        if *due
            && next.is_some_and(|s| {
                !matches!(
                    s,
                    Step::Settle(_)
                        | Step::ControlWait(_)
                        | Step::WaitScreen(_)
                        | Step::WaitPane(_)
                )
            })
        {
            *due = !self.upgrade(i, true);
        }
    }

    /// The server moved onto `bin` again over the frozen handshake
    /// (`UpgradeRequest`, as `vtmux-upgrade -b <bin>` from a client of
    /// another version asks; unlike the command it leaves no line in
    /// `show-messages`), then every attached client back from its re-exec
    /// and the redraw it gets quiet. Observes nothing when that goes
    /// through, and what went wrong otherwise (which no trace holds, so it
    /// fails the step). Inserted (`auto`), it first lets what the steps
    /// before provoked settle, and is skipped while no server listens. Put
    /// off (false) when the server's strict hook (`--upgrade-every` runs
    /// only) says not now.
    fn upgrade(&mut self, step: usize, auto: bool) -> bool {
        if auto {
            self.settle(DEFAULT_QUIET);
        }
        let marks: Vec<(String, usize)> = self
            .attaches
            .iter_mut()
            .filter_map(|(id, a)| {
                matches!(a.child.try_wait(), Ok(None))
                    .then(|| (id.clone(), a.events.lock().unwrap().len()))
            })
            .collect();
        let what = if auto {
            "the upgrade after this step"
        } else {
            "upgrade"
        };
        let failed = match self.upgrade_request() {
            Ok(None) if auto => return true,
            Ok(None) => Some(format!("{what}: no server")),
            Ok(Some((true, message))) if message.is_empty() => None,
            // Under the strict hook an `upgrade` step of the scenario's own
            // is put off too: plain `replay` runs it where it stands.
            Ok(Some((false, message))) if message.starts_with(STRICT_REFUSAL) => {
                DEFERRED.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            Ok(Some((ok, message))) => Some(format!("{what}: ok={ok}: {message}")),
            Err(e) => Some(format!("{what}: {e}")),
        };
        if auto {
            UPGRADES.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(message) = failed {
            let observation = Observation::Cli {
                stdout: String::new(),
                stderr: self.norm.text(&format!("{message}\n")),
                exit: 1,
            };
            self.items.push(Item {
                step,
                observation,
                divergence: None,
            });
            return true;
        }
        for (id, at) in marks {
            if !self.reattached(&id, at) {
                self.items.push(Item {
                    step,
                    observation: Observation::Timeout {
                        what: format!("{what}: attached client {id:?} re-attaching"),
                    },
                    divergence: None,
                });
            }
        }
        self.settle(DEFAULT_QUIET);
        true
    }

    /// The frozen upgrade handshake onto `bin`: the server's answer, or
    /// `None` when nothing listens on the socket.
    fn upgrade_request(&self) -> std::result::Result<Option<(bool, String)>, String> {
        use vtmux_core::protocol::{self, ClientMsg, ServerMsg};
        let Ok(mut stream) = UnixStream::connect(&self.socket) else {
            return Ok(None);
        };
        stream
            .set_read_timeout(Some(UPGRADE_TIMEOUT))
            .map_err(|e| e.to_string())?;
        let request = ClientMsg::UpgradeRequest {
            binary: self.bin.to_path_buf(),
        };
        protocol::write_msg(&mut stream, &request).map_err(|e| e.to_string())?;
        match protocol::read_msg::<ServerMsg>(&mut stream) {
            Ok(Some(ServerMsg::UpgradeResult { ok, message })) => Ok(Some((ok, message))),
            Ok(Some(other)) => Err(format!("answered {other:?}")),
            Ok(None) => Err("the connection closed with no answer".into()),
            Err(e) => Err(format!("no answer: {e}")),
        }
    }

    /// Attached client `id` has been sent, since event `at`, what a
    /// re-attached client gets first ([`REATTACHED`]).
    fn reattached(&self, id: &str, at: usize) -> bool {
        let start = Instant::now();
        loop {
            let bytes: Vec<u8> = self.attaches[id].events.lock().unwrap()[at..]
                .iter()
                .filter_map(|e| match e {
                    Event::Bytes(b) => Some(b.as_slice()),
                    Event::Resize(..) => None,
                })
                .flatten()
                .copied()
                .collect();
            if bytes.windows(REATTACHED.len()).any(|w| w == REATTACHED) {
                return true;
            }
            if start.elapsed() > REATTACH_LIMIT {
                return false;
            }
            thread::sleep(POLL);
        }
    }

    fn stale_socket(&self) -> Result<()> {
        // Bound then closed: the file stays, nothing listens. A child another
        // scenario's thread is spawning holds a copy of the listener until it
        // execs, and a client that connects meanwhile loses the socket
        // ("server exited unexpectedly"), so wait until a connection is
        // refused.
        drop(UnixListener::bind(&self.socket).map_err(|e| e.to_string())?);
        let start = Instant::now();
        while UnixStream::connect(&self.socket).is_ok() {
            if start.elapsed() > Duration::from_secs(5) {
                return Err("stale socket stays live".into());
            }
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    fn control_mut(&mut self, id: &str) -> Result<&mut Control> {
        self.controls
            .get_mut(id)
            .ok_or_else(|| format!("no control client {id:?}"))
    }

    fn control_start(&mut self, id: &str, args: &[String]) -> Result<()> {
        let mut cmd = self.command(&[]);
        cmd.arg("-C")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("{}: {e}", self.bin.display()))?;
        let out = Arc::new(Mutex::new(Vec::new()));
        let err = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let (o, e) = (Arc::clone(&out), Arc::clone(&err));
        spawn_reader(
            child.stdout.take().unwrap(),
            Arc::clone(&self.act),
            Arc::clone(&done),
            move |b| o.lock().unwrap().extend_from_slice(b),
        );
        spawn_reader(
            child.stderr.take().unwrap(),
            Arc::clone(&self.act),
            Arc::new(AtomicBool::new(false)),
            move |b| e.lock().unwrap().extend_from_slice(b),
        );
        let stdin = child.stdin.take();
        self.controls.insert(
            id.to_string(),
            Control {
                child,
                stdin,
                out,
                err,
                done,
                mark: 0,
            },
        );
        // Sync on the attach notification, or on exit for an unattached client.
        let ctl = &self.controls[id];
        let start = Instant::now();
        while !(has_line(&ctl.out.lock().unwrap(), 0, "%session-changed")
            || ctl.done.load(Ordering::SeqCst))
        {
            if start.elapsed() > WAIT_TIMEOUT {
                return Err(format!(
                    "control {id}: no %session-changed within {WAIT_TIMEOUT:?}"
                ));
            }
            thread::sleep(POLL);
        }
        Ok(())
    }

    fn control_wait(&mut self, id: &str, until: &str) -> Result<bool> {
        let ctl = self.control_mut(id)?;
        let start = Instant::now();
        loop {
            if has_line(&ctl.out.lock().unwrap(), ctl.mark, until) {
                return Ok(true);
            }
            if start.elapsed() > WAIT_TIMEOUT || ctl.done.load(Ordering::SeqCst) {
                return Ok(false);
            }
            thread::sleep(POLL);
        }
    }

    fn control_close(&mut self, step: usize, id: &str) -> Result<()> {
        let mut ctl = self
            .controls
            .remove(id)
            .ok_or_else(|| format!("no control client {id:?}"))?;
        drop(ctl.stdin.take());
        let start = Instant::now();
        let status = loop {
            if let Some(s) = ctl.child.try_wait().map_err(|e| e.to_string())? {
                break Some(s);
            }
            if start.elapsed() > WAIT_TIMEOUT {
                let _ = ctl.child.kill();
                let _ = ctl.child.wait();
                break None;
            }
            thread::sleep(POLL);
        };
        let limit = Instant::now() + DRAIN_LIMIT;
        while !ctl.done.load(Ordering::SeqCst) && Instant::now() < limit {
            thread::sleep(POLL);
        }
        let obs = match status {
            Some(s) => {
                let out = String::from_utf8_lossy(&ctl.out.lock().unwrap()).into_owned();
                let err = String::from_utf8_lossy(&ctl.err.lock().unwrap()).into_owned();
                Observation::Control {
                    lines: self.norm.stream(&out),
                    stderr: self.norm.text(&err),
                    exit: s.code().unwrap_or(-1),
                }
            }
            None => Observation::Timeout {
                what: format!("control_close {id}"),
            },
        };
        self.push(step, obs);
        Ok(())
    }

    /// Raw bytes to the attached client `id`.
    fn keys(&mut self, id: &str, k: &str) -> Result<()> {
        let total = self.act.total.load(Ordering::SeqCst);
        let a = self
            .attaches
            .get_mut(id)
            .ok_or_else(|| format!("keys: no attached client {id:?}"))?;
        a.writer
            .write_all(k.as_bytes())
            .map_err(|e| e.to_string())?;
        a.writer.flush().map_err(|e| e.to_string())?;
        self.expect_after = Some(total);
        Ok(())
    }

    fn attach_start(&mut self, session: &str, id: String, term: Option<&str>) -> Result<()> {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: self.size[1],
                cols: self.size[0],
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())?;
        let mut cmd = CommandBuilder::new(self.bin);
        cmd.env_clear();
        for (k, v) in self.env() {
            cmd.env(k, v);
        }
        if let Some(term) = term {
            cmd.env("TERM", term);
        }
        cmd.args(self.base_args());
        cmd.args(["attach", "-t", session]);
        cmd.cwd(self.root.join("home"));
        let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
        drop(pair.slave);
        // The observer terminal starts at the scenario size: a client
        // started after a resize starts at the current one.
        let mut start = Vec::new();
        if self.size != self.sc.size {
            start.push(Event::Resize(self.size[0], self.size[1]));
        }
        let events = Arc::new(Mutex::new(start));
        let ev = Arc::clone(&events);
        let reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        spawn_reader(
            reader,
            Arc::clone(&self.act),
            Arc::new(AtomicBool::new(false)),
            move |b| {
                ev.lock().unwrap().push(Event::Bytes(b.to_vec()));
            },
        );
        let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
        self.expect_after = Some(self.act.total.load(Ordering::SeqCst));
        let mut attach = Attach {
            master: pair.master,
            writer,
            child,
            events,
        };
        // The server lists clients in connect order (server_client_create,
        // server-client.c:320), and a client draws only after the server
        // answered its attach. Wait for that first draw (or an early exit)
        // so two attaches never race to connect.
        let start = Instant::now();
        while start.elapsed() < WAIT_TIMEOUT
            && !attach
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, Event::Bytes(_)))
            && matches!(attach.child.try_wait(), Ok(None))
        {
            thread::sleep(POLL);
        }
        self.attaches.insert(id, attach);
        Ok(())
    }

    /// Quiescence: wait for the output an input provoked (if any), then for
    /// `quiet` without output from any client. False if it never settles.
    fn settle(&mut self, quiet: Duration) -> bool {
        let has_client = !self.attaches.is_empty() || !self.controls.is_empty();
        if let Some(before) = self.expect_after.take().filter(|_| has_client) {
            let start = Instant::now();
            while self.act.total.load(Ordering::SeqCst) == before
                && start.elapsed() < REACTION_LIMIT
            {
                thread::sleep(POLL);
            }
        }
        let start = Instant::now();
        loop {
            if self.act.last.lock().unwrap().elapsed() >= quiet {
                self.baseline = self.act.total.load(Ordering::SeqCst);
                return true;
            }
            if start.elapsed() > SETTLE_LIMIT {
                return false;
            }
            thread::sleep(POLL);
        }
    }

    /// The attached client's output fed to an observer terminal; grids are
    /// compared, never the raw bytes (spec, *Behaviour oracle*).
    /// The attached client's output so far, fed into a terminal of its size.
    fn client_term(&self, id: &str) -> Result<PaneTerm> {
        let a = self
            .attaches
            .get(id)
            .ok_or_else(|| format!("no attached client {id:?}"))?;
        let mut term = PaneTerm::new(self.sc.size[0], self.sc.size[1], HISTORY_LIMIT);
        for e in a.events.lock().unwrap().iter() {
            match e {
                Event::Bytes(b) => term.feed(b),
                Event::Resize(c, r) => term.resize(*c, *r),
            }
        }
        Ok(term)
    }

    fn settle_step(&mut self, step: usize, s: &Settle) {
        if s.react && self.expect_after.is_none() {
            self.expect_after = Some(self.baseline);
        }
        if !self.settle(s.quiet_ms.map_or(DEFAULT_QUIET, Duration::from_millis)) {
            self.push(
                step,
                Observation::Timeout {
                    what: "settle".into(),
                },
            );
        }
    }

    /// The `wait_screen` step: a `timeout` observation if it never gets there.
    fn wait_screen_step(&mut self, step: usize, w: &WaitScreen) -> Result<()> {
        if !self.wait_screen(w)? {
            self.push(
                step,
                Observation::Timeout {
                    what: format!("wait_screen {w:?}"),
                },
            );
        }
        Ok(())
    }

    /// The `wait_pane` step: poll `capture-pane -p` until it prints `has`; a
    /// `timeout` observation if it never does within `WAIT_SCREEN_LIMIT`.
    fn wait_pane(&mut self, step: usize, w: &WaitPane) -> Result<()> {
        let argv: Vec<String> = ["capture-pane", "-p", "-t", &w.pane]
            .map(String::from)
            .to_vec();
        let start = Instant::now();
        loop {
            if let Some((out, _, 0)) = self.cli_raw(&argv, CLI_TIMEOUT)?
                && out.contains(&w.has)
            {
                return Ok(());
            }
            if start.elapsed() > WAIT_SCREEN_LIMIT {
                self.push(
                    step,
                    Observation::Timeout {
                        what: format!("wait_pane {w:?}"),
                    },
                );
                return Ok(());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Poll the client's screen until it shows every `has` and none of the
    /// `lacks` texts. False if it never does within `WAIT_SCREEN_LIMIT`.
    fn wait_screen(&self, w: &WaitScreen) -> Result<bool> {
        let id = w.id.clone().unwrap_or_default();
        let start = Instant::now();
        loop {
            let term = self.client_term(&id)?;
            let text =
                String::from_utf8_lossy(&capture(&term, CaptureOpts::default())).into_owned();
            let rows: Vec<&str> = text.lines().collect();
            let shown = |t: &String| rows.iter().any(|r| r.contains(t.as_str()));
            if w.has.iter().all(shown) && !w.lacks.iter().any(shown) {
                return Ok(true);
            }
            if start.elapsed() > WAIT_SCREEN_LIMIT {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The title the attached client's terminal ends up with, as a
    /// terminal reads it (xterm: OSC 0 or 2, ended by BEL or ST); not a
    /// pane's rules, under which an APC string is a title too.
    fn client_title(&self, id: &str) -> Result<String> {
        let a = self
            .attaches
            .get(id)
            .ok_or_else(|| format!("no attached client {id:?}"))?;
        let bytes: Vec<u8> = a
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Event::Bytes(b) => Some(b.as_slice()),
                Event::Resize(..) => None,
            })
            .flatten()
            .copied()
            .collect();
        Ok(terminal_title(&bytes))
    }

    /// Settle, then observe the attached client's screen, or its terminal
    /// title or progress bar as a `cli` observation.
    fn observe_client(&mut self, step: usize, id: &str, view: ClientView) -> Result<()> {
        let cli = |stdout: String| Observation::Cli {
            stdout,
            stderr: String::new(),
            exit: 0,
        };
        let obs = if self.settle(DEFAULT_QUIET) {
            match view {
                ClientView::Screen => self.screen(id)?,
                ClientView::Title => {
                    let title = self.client_title(id)?;
                    cli(self.norm.text(&format!("{title}\n")))
                }
                ClientView::Progress => {
                    let bar = self.client_term(id)?.progress_bar();
                    cli(format!("{}|{}\n", bar.state.name(), bar.progress))
                }
            }
        } else {
            Observation::Timeout {
                what: format!("settle before {}", view.name()),
            }
        };
        self.push(step, obs);
        Ok(())
    }

    fn screen(&self, id: &str) -> Result<Observation> {
        let term = self.client_term(id)?;
        let bytes = capture(
            &term,
            CaptureOpts {
                escapes: true,
                ..CaptureOpts::default()
            },
        );
        let text = String::from_utf8_lossy(&bytes).replace('\u{1b}', "\\e");
        let mut rows: Vec<String> = text.split('\n').map(|r| self.norm.row(r)).collect();
        if rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        let (x, y) = term.cursor();
        Ok(Observation::Screen {
            rows,
            cursor: [x, y],
        })
    }
}

/// The last title an OSC 0 or 2 in `bytes` set (`OSC Ps ; Pt`, ended by
/// BEL or ESC), as a terminal keeps it.
fn terminal_title(bytes: &[u8]) -> String {
    let mut title = String::new();
    let mut rest = bytes;
    while let Some(at) = rest.windows(2).position(|w| w == b"\x1b]") {
        rest = &rest[at + 2..];
        let end = rest
            .iter()
            .position(|b| matches!(b, 0x07 | 0x1b))
            .unwrap_or(rest.len());
        let osc = &rest[..end];
        let digits = osc.iter().take_while(|b| b.is_ascii_digit()).count();
        if matches!(&osc[..digits], b"0" | b"2")
            && osc.get(digits) == Some(&b';')
            && end < rest.len()
        {
            title = String::from_utf8_lossy(&osc[digits + 1..]).into_owned();
        }
        rest = &rest[end..];
    }
    title
}

/// True if a line of `buf[from..]` starts with `prefix`.
fn has_line(buf: &[u8], from: usize, prefix: &str) -> bool {
    let from = from.min(buf.len());
    buf[from..]
        .split(|b| *b == b'\n')
        .any(|l| l.starts_with(prefix.as_bytes()))
}

/// What [`Runner::observe_client`] reads from an attached client's
/// terminal.
#[derive(Clone, Copy)]
enum ClientView {
    Screen,
    Title,
    Progress,
}

impl ClientView {
    fn name(self) -> &'static str {
        match self {
            Self::Screen => "screen",
            Self::Title => "title",
            Self::Progress => "progress",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_root_ignores_tmpdir_and_stays_short() {
        // `make_root` never consults $TMPDIR, so a long one cannot lengthen the path.
        let root = make_root().unwrap();
        let socket = socket_path(&root);
        let _ = fs::remove_dir_all(&root);
        let socket = socket.unwrap();
        assert!(socket.starts_with(ROOT_BASE), "{}", socket.display());
        assert_eq!(socket.as_os_str().len(), "/tmp/vto-xxxxxx/s".len());
    }

    #[test]
    fn terminal_title_is_the_last_osc_0_or_2() {
        assert_eq!(terminal_title(b"x\x1b]2;one\x07\x1b]0;two\x1b\\y"), "two");
        assert_eq!(
            terminal_title(b"\x1b]2;a\x07\x1b_Gx\x1b\\\x1b]1;icon\x07"),
            "a"
        );
        assert_eq!(terminal_title(b"\x1b]2;a\x07\x1b]2;unended"), "a");
    }

    #[test]
    fn overlong_socket_path_is_refused() {
        let root = Path::new("/tmp").join("y".repeat(MAX_SOCKET_LEN));
        assert!(socket_path(&root).is_err());
        assert!(socket_path(Path::new("/tmp/vto-abcdef")).is_ok());
    }
}
