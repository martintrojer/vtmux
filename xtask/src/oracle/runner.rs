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
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use vtmux_vt::{CaptureOpts, PaneTerm, capture};

use super::normalize::Normalizer;
use super::scenario::{Divergence, Observation, Scenario, Setup, Step};

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

const CLI_TIMEOUT: Duration = Duration::from_secs(10);
const WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_QUIET: Duration = Duration::from_millis(150);
const SETTLE_LIMIT: Duration = Duration::from_secs(10);
/// How long `settle` waits for the first output a just-sent input provokes.
const REACTION_LIMIT: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(5);
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

pub struct Runner<'a> {
    bin: &'a Path,
    sc: &'a Scenario,
    root: PathBuf,
    socket: PathBuf,
    norm: Normalizer,
    act: Arc<Activity>,
    controls: HashMap<String, Control>,
    attach: Option<Attach>,
    size: [u16; 2],
    items: Vec<Item>,
    pending_div: Option<Divergence>,
    /// Output total when an input was sent that should provoke output.
    expect_after: Option<usize>,
}

impl Drop for Runner<'_> {
    fn drop(&mut self) {
        // Best effort: stop the server this run started, then every client.
        let _ = self.cli_raw(&["kill-server".into()], Duration::from_secs(5));
        for c in self.controls.values_mut() {
            let _ = c.child.kill();
            let _ = c.child.wait();
        }
        if let Some(a) = self.attach.as_mut() {
            let _ = a.child.kill();
            let _ = a.child.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Run `sc` against `bin` and return its normalised observations.
pub fn run(bin: &Path, sc: &Scenario) -> Result<Vec<Item>> {
    let mut r = Runner::new(bin, sc)?;
    r.run_steps()?;
    Ok(std::mem::take(&mut r.items))
}

impl<'a> Runner<'a> {
    fn new(bin: &'a Path, sc: &'a Scenario) -> Result<Self> {
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
            root,
            socket,
            norm,
            act: Arc::new(Activity {
                last: Mutex::new(Instant::now()),
                total: AtomicUsize::new(0),
            }),
            controls: HashMap::new(),
            attach: None,
            size: sc.size,
            items: Vec::new(),
            pending_div: None,
            expect_after: None,
        })
    }

    fn init_root(bin: &Path, root: &Path) -> Result<()> {
        fs::create_dir(root.join("home")).map_err(|e| format!("{}: {e}", root.display()))?;
        // Pane programs signal with `tmux wait-for -S`; $TMUX gives them the socket.
        fs::create_dir(root.join("bin")).map_err(|e| e.to_string())?;
        std::os::unix::fs::symlink(bin, root.join("bin/tmux")).map_err(|e| e.to_string())
    }

    fn env(&self) -> Vec<(&'static str, String)> {
        vec![
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
        ]
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
        // The pipes close with the process unless a daemon inherited them.
        let limit = Instant::now() + Duration::from_secs(2);
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
                Step::Attach(a) => self.attach_start(&a.session)?,
                Step::Keys(k) => {
                    let total = self.act.total.load(Ordering::SeqCst);
                    let a = self.attach.as_mut().ok_or("keys: no attached client")?;
                    a.writer
                        .write_all(k.as_bytes())
                        .map_err(|e| e.to_string())?;
                    a.writer.flush().map_err(|e| e.to_string())?;
                    self.expect_after = Some(total);
                }
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
                    let a = self.attach.as_mut().ok_or("resize: no attached client")?;
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
                Step::Settle(s) => {
                    if !self.settle(s.quiet_ms.map_or(DEFAULT_QUIET, Duration::from_millis)) {
                        self.push(
                            i,
                            Observation::Timeout {
                                what: "settle".into(),
                            },
                        );
                    }
                }
                Step::WaitFor(chan) => {
                    let argv = vec!["wait-for".to_string(), chan.clone()];
                    self.cli(i, &argv, &format!("wait_for {chan}"))?;
                }
                Step::Screen(_) => {
                    if self.settle(DEFAULT_QUIET) {
                        let obs = self.screen()?;
                        self.push(i, obs);
                    } else {
                        self.push(
                            i,
                            Observation::Timeout {
                                what: "settle before screen".into(),
                            },
                        );
                    }
                }
                Step::Divergence(d) => self.pending_div = Some(d.clone()),
            }
        }
        if self.pending_div.is_some() {
            return Err("divergence step is not followed by an observation".into());
        }
        Ok(())
    }

    fn stale_socket(&self) -> Result<()> {
        // Bound then closed: the file stays, nothing listens.
        drop(UnixListener::bind(&self.socket).map_err(|e| e.to_string())?);
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
        let limit = Instant::now() + Duration::from_secs(2);
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

    fn attach_start(&mut self, session: &str) -> Result<()> {
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
        cmd.args(self.base_args());
        cmd.args(["attach", "-t", session]);
        cmd.cwd(self.root.join("home"));
        let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
        drop(pair.slave);
        let events = Arc::new(Mutex::new(Vec::new()));
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
        self.attach = Some(Attach {
            master: pair.master,
            writer,
            child,
            events,
        });
        Ok(())
    }

    /// Quiescence: wait for the output an input provoked (if any), then for
    /// `quiet` without output from any client. False if it never settles.
    fn settle(&mut self, quiet: Duration) -> bool {
        let has_client = self.attach.is_some() || !self.controls.is_empty();
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
    fn screen(&self) -> Result<Observation> {
        let a = self.attach.as_ref().ok_or("screen: no attached client")?;
        let mut term = PaneTerm::new(self.sc.size[0], self.sc.size[1], HISTORY_LIMIT);
        for e in a.events.lock().unwrap().iter() {
            match e {
                Event::Bytes(b) => term.feed(b),
                Event::Resize(c, r) => term.resize(*c, *r),
            }
        }
        let bytes = capture(
            &term,
            CaptureOpts {
                escapes: true,
                ..CaptureOpts::default()
            },
        );
        let text = String::from_utf8_lossy(&bytes).replace('\u{1b}', "\\e");
        let mut rows: Vec<String> = text.split('\n').map(|r| self.norm.text(r)).collect();
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

/// True if a line of `buf[from..]` starts with `prefix`.
fn has_line(buf: &[u8], from: usize, prefix: &str) -> bool {
    let from = from.min(buf.len());
    buf[from..]
        .split(|b| *b == b'\n')
        .any(|l| l.starts_with(prefix.as_bytes()))
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
    fn overlong_socket_path_is_refused() {
        let root = Path::new("/tmp").join("y".repeat(MAX_SOCKET_LEN));
        assert!(socket_path(&root).is_err());
        assert!(socket_path(Path::new("/tmp/vto-abcdef")).is_ok());
    }
}
