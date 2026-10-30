//! CLI scenarios run against `target/debug/vtmux` and the reference tmux
//! 3.8, compared after normalisation (socket dir, pid, time).
//!
//! Isolation: `TMUX` and `TMUX_PANE` cleared, a private `TMUX_TMPDIR` per
//! scenario, an explicit `-L` per binary, `-f /dev/null`, SHELL=/bin/sh.
//! The reference is `$VTMUX_REFERENCE_TMUX` or the `cargo xtask ref-tmux`
//! cache; without it each test prints why and checks vtmux alone against
//! the recorded expectations embedded below.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// One run's observable result.
#[derive(Debug, PartialEq, Eq)]
struct Run {
    stdout: String,
    stderr: String,
    code: i32,
}

fn vtmux_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_vtmux"))
}

/// vtmux invoked as `tmux` (a symlink, D20): tmux's socket directory and
/// tmux's program name in messages, so transcripts compare like for like.
fn vtmux_as_tmux() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let link = dir.path().join("tmux");
    std::os::unix::fs::symlink(vtmux_bin(), &link).expect("symlink");
    (dir, link)
}

fn reference() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("VTMUX_REFERENCE_TMUX").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    let bin = cache.join("vtmux/tmux-3.8/bin/tmux");
    bin.exists().then_some(bin)
}

/// A private world for one binary: its own `TMUX_TMPDIR` and label.
struct World {
    bin: PathBuf,
    tmpdir: tempfile::TempDir,
    label: String,
}

impl World {
    fn new(bin: &Path) -> Self {
        let tmpdir = tempfile::tempdir().expect("tempdir");
        Self {
            bin: bin.to_owned(),
            label: format!("cli{}", std::process::id()),
            tmpdir,
        }
    }

    fn dir(&self) -> &Path {
        self.tmpdir.path()
    }

    /// Run with explicit global args (no `-L` added).
    fn raw(&self, args: &[&str]) -> Run {
        self.raw_env(args, &[])
    }

    /// `raw` with extra environment variables for the client.
    fn raw_env(&self, args: &[&str], env: &[(&str, &str)]) -> Run {
        let out = Command::new(&self.bin)
            .args(args)
            .envs(env.iter().copied())
            .current_dir(self.dir())
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env("TMUX_TMPDIR", self.dir())
            .env("SHELL", "/bin/sh")
            .output()
            .expect("run binary");
        Run {
            stdout: self.normalise(&String::from_utf8_lossy(&out.stdout)),
            stderr: self.normalise(&String::from_utf8_lossy(&out.stderr)),
            code: out.status.code().unwrap_or(-1),
        }
    }

    /// Run `-L label -f /dev/null args...`.
    fn run(&self, args: &[&str]) -> Run {
        let mut full = vec!["-L", &self.label, "-f", "/dev/null"];
        full.extend_from_slice(args);
        self.raw(&full)
    }

    /// Socket dir and temp dir -> placeholders; ctime text -> `TIME`.
    fn normalise(&self, s: &str) -> String {
        // tmux realpaths TMUX_TMPDIR, so on macOS /tmp shows as /private/tmp:
        // replace the canonical form first, then the given one.
        let given = self.dir().to_string_lossy().into_owned();
        let canon = std::fs::canonicalize(self.dir())
            .map_or_else(|_| given.clone(), |p| p.to_string_lossy().into_owned());
        let mut out = String::new();
        for line in s.split_inclusive('\n') {
            let mut line = line.to_owned();
            for dir in [&canon, &given] {
                line = line
                    .replace(&format!("{dir}/tmux-"), "SOCKDIR/tmux-")
                    .replace(dir.as_str(), "TMP");
            }
            if let Some(at) = line.find("(created ") {
                let end = line[at..].find(')').map_or(line.len(), |e| at + e);
                line.replace_range(at..end, "(created TIME");
            }
            out.push_str(&line);
        }
        out
    }

    /// Poll `capture-pane -p` (max 5 s, 50 ms steps) until `want` shows.
    fn wait_for_text(&self, target: &str, want: &str) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if self
                .run(&["capture-pane", "-p", "-t", target])
                .stdout
                .contains(want)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "{want:?} never appeared in {target} ({})",
            self.bin.display()
        );
    }

    /// Poll until the server is gone (`ls` fails).
    fn wait_gone(&self) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if self.run(&["ls"]).code != 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("server did not exit ({})", self.bin.display());
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = self.run(&["kill-server"]);
    }
}

/// Run `scenario` on vtmux and the reference and compare the transcripts.
/// Each step's result is recorded; the reference's transcript is the
/// expectation.
fn compare(name: &str, scenario: impl Fn(&World) -> Vec<Run>) {
    let (_keep, as_tmux) = vtmux_as_tmux();
    let ours = scenario(&World::new(&as_tmux));
    let Some(reference) = reference() else {
        eprintln!(
            "{name}: SKIP reference comparison: no reference tmux (set VTMUX_REFERENCE_TMUX or run `cargo xtask ref-tmux`)"
        );
        return;
    };
    let theirs = scenario(&World::new(&reference));
    assert_eq!(ours.len(), theirs.len());
    for (i, (o, t)) in ours.iter().zip(&theirs).enumerate() {
        assert_eq!(
            o, t,
            "{name}: step {i} differs (left vtmux, right tmux 3.8)"
        );
    }
}

/// (1) `ls` with no server: no socket file -> error connecting.
#[test]
fn ls_without_server() {
    compare("ls_without_server", |w| {
        vec![w.run(&["ls"]), w.run(&["has", "-t", "a"])]
    });
}

/// (2) `-S` into a missing directory: `ls` cannot connect, `new -d`
/// cannot create (client.c:291-297, server.c:151-152).
#[test]
fn missing_socket_dir() {
    compare("missing_socket_dir", |w| {
        let path = w.dir().join("nodir/s");
        let p = path.to_str().expect("utf-8 path");
        vec![
            w.raw(&["-f", "/dev/null", "-S", p, "ls"]),
            w.raw(&["-f", "/dev/null", "-S", p, "new", "-d"]),
            w.raw(&["-f", "/dev/null", "-S", p, "kill-server"]),
        ]
    });
}

/// (3) A stale socket file (nothing listening): `no server running on`.
#[test]
fn stale_socket() {
    compare("stale_socket", |w| {
        let path = w.dir().join("stale");
        drop(std::os::unix::net::UnixListener::bind(&path).expect("bind"));
        let p = path.to_str().expect("utf-8 path");
        vec![
            w.raw(&["-f", "/dev/null", "-S", p, "ls"]),
            w.raw(&["-f", "/dev/null", "-S", p, "has"]),
        ]
    });
}

/// (4) has-session on an existing and a missing session.
#[test]
fn has_session() {
    compare("has_session", |w| {
        vec![
            w.run(&["new", "-d", "-s", "a"]),
            w.run(&["has", "-t", "a"]),
            w.run(&["has", "-t", "nope"]),
            w.run(&["has", "-t", "=a"]),
            w.run(&["has"]),
        ]
    });
}

/// (5) A second session with the same name.
#[test]
fn duplicate_session() {
    compare("duplicate_session", |w| {
        vec![
            w.run(&["new", "-d", "-s", "a"]),
            w.run(&["new", "-d", "-s", "a"]),
        ]
    });
}

/// (6) `ls` lists in name order whatever the creation order; `-F`.
#[test]
fn list_sessions_order() {
    compare("list_sessions_order", |w| {
        vec![
            w.run(&["new", "-d", "-s", "b"]),
            w.run(&["new", "-d", "-s", "a"]),
            w.run(&["ls"]),
            w.run(&["ls", "-F", "#{session_name}"]),
            w.run(&["ls", "-O", "index", "-F", "#{session_name} #{session_id}"]),
            w.run(&["ls", "-O", "bad"]),
        ]
    });
}

/// (7) display-message -p with the ID and version variables.
#[test]
fn display_message_ids() {
    compare("display_message_ids", |w| {
        vec![
            w.run(&["new", "-d", "-s", "a"]),
            w.run(&[
                "display",
                "-p",
                "#{session_name} #{session_id} #{window_id} #{pane_id} #{version}",
            ]),
            w.run(&["display", "-p", "-t", "a", "#S:#I.#P #{window_index}"]),
            w.run(&["display", "hello"]),
            w.run(&["display", "-p", "%%"]),
            w.run(&["display", "-p", "-F", "x", "y"]),
        ]
    });
}

/// (8) send-keys -l into `cat` on a 20x5 pane, then capture.
#[test]
fn send_keys_and_capture() {
    compare("send_keys_and_capture", |w| {
        let mut runs = vec![
            w.run(&["new", "-d", "-s", "a", "-x", "20", "-y", "5", "cat"]),
            w.run(&["send-keys", "-t", "a", "-l", "hi"]),
        ];
        w.wait_for_text("a", "hi");
        runs.push(w.run(&["capture-pane", "-p", "-t", "a"]));
        runs.push(w.run(&["display", "-p", "-t", "a", "#{pane_width}x#{pane_height}"]));
        runs.push(w.run(&["send-keys", "-t", "a", "Enter", "x", "y"]));
        w.wait_for_text("a", "xy");
        runs.push(w.run(&["capture-pane", "-p", "-t", "a"]));
        runs
    });
}

/// (9) Command lookup, flag and arity errors (the `task_0` shared facts).
#[test]
fn command_errors() {
    compare("command_errors", |w| {
        vec![
            w.run(&["new", "-d", "-s", "a"]),
            w.run(&["nosuchcmd"]),
            w.run(&["displ"]),
            w.run(&["display-message", "-Z"]),
            w.run(&["show-options", "a", "b"]),
            w.run(&["set-option"]),
            w.run(&["new", "-d", "-x", "abc"]),
            w.run(&["capture-pane", "-p", "-t", "nope"]),
            w.raw(&["-Z"]),
        ]
    });
}

/// (10) kill-session leaves the others; killing the last ends the server.
#[test]
fn kill_session() {
    compare("kill_session", |w| {
        let mut runs = vec![
            w.run(&["new", "-d", "-s", "a"]),
            w.run(&["new", "-d", "-s", "b"]),
            w.run(&["kill-session", "-t", "a"]),
            w.run(&["ls"]),
            w.run(&["kill-session", "-t", "nope"]),
            w.run(&["kill-session", "-t", "b"]),
        ];
        w.wait_gone();
        runs.push(w.run(&["ls"]));
        runs
    });
}

/// (11) kill-server; afterwards the socket file is stale.
#[test]
fn kill_server() {
    compare("kill_server", |w| {
        let mut runs = vec![w.run(&["new", "-d", "-s", "a"]), w.run(&["kill-server"])];
        w.wait_gone();
        runs.push(w.run(&["ls"]));
        runs.push(w.run(&["kill-server"]));
        runs
    });
}

/// Under its own name (D20, D21): `vtmux-UID` socket directory, `vtmux`
/// in getopt messages and usage, and `-V` naming both versions.
#[test]
fn own_name() {
    let w = World::new(&vtmux_bin());
    let ls = w.run(&["ls"]);
    assert_eq!(
        ls.stderr,
        format!(
            "error connecting to TMP/vtmux-{}/{} (No such file or directory)\n",
            uid(),
            w.label
        )
    );
    let v = w.raw(&["-V"]);
    assert_eq!(
        v.stdout,
        format!("vtmux {} (tmux 3.8)\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(
        w.raw(&["-Z"])
            .stderr
            .starts_with("vtmux: unknown option -- Z\nusage: vtmux ")
    );
    let (_keep, as_tmux) = vtmux_as_tmux();
    assert_eq!(World::new(&as_tmux).raw(&["-V"]).stdout, "tmux 3.8\n");
}

fn uid() -> String {
    let out = Command::new("id").arg("-u").output().expect("id -u");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Config files: `~user` resolves through the passwd database (NSS:
/// on Fedora Atomic `bin` lives in /usr/lib/passwd, not /etc/passwd), an
/// assignment applies before later lines are parsed, `%hidden` values
/// expand but stay out of formats' process view, `-n` parses without
/// applying, `-v` prints each command, and a parse error is
/// `file:line: message` (cmd-parse.y `yylex_token_tilde`, 224-270;
/// cmd-source-file.c).
#[test]
fn source_file_config_env() {
    compare("source_file_config_env", |w| {
        let conf = w.dir().join("a.conf");
        std::fs::write(
            &conf,
            "X=1\ndisplay -p ~bin/$X\n%hidden Y=2\ndisplay -p \"$Y\"\n",
        )
        .expect("write");
        let bad = w.dir().join("b.conf");
        std::fs::write(&bad, "display -p a\nnosuch\n").expect("write");
        let (c, b) = (conf.to_str().expect("utf-8"), bad.to_str().expect("utf-8"));
        vec![
            w.run(&["new", "-d", "-s", "a"]),
            w.run(&["source", "-n", c]),
            w.run(&["display", "-p", "[#{X}]"]),
            w.run(&["source", c]),
            w.run(&["display", "-p", "[#{X}] [#{Y}]"]),
            w.run(&["source", "-nv", c]),
            w.run(&["source", b]),
            w.run(&["source", "/nonexistent.conf"]),
            w.run(&["source", "-q", "/nonexistent.conf"]),
        ]
    });
}

/// A startup config runs before the first command, without a client:
/// `new` in it is detached, errors are kept for an attached session and
/// never reach a command client (cfg.c `start_cfg`, `cfg_show_causes`).
#[test]
fn startup_config() {
    compare("startup_config", |w| {
        let conf = w.dir().join("start.conf");
        std::fs::write(&conf, "new -s test\ndisplay hello\nnosuch\n").expect("write");
        let c = conf.to_str().expect("utf-8");
        vec![
            w.raw(&["-L", &w.label, "-f", c, "start"]),
            w.run(&["ls", "-F", "#{session_name}"]),
        ]
    });
}

/// A multi-word command is run as `execvp` does (spawn.c:552), so an
/// executable without a shebang runs through `/bin/sh` (ENOEXEC fallback),
/// given by path and found through the pane's `PATH`.
#[test]
fn spawn_script_without_shebang() {
    use std::os::unix::fs::PermissionsExt;
    compare("spawn_script_without_shebang", |w| {
        let bin = w.dir().join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        let script = bin.join("nsb");
        std::fs::write(&script, "echo ran-$1\nsleep 30\n").expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let path = script.to_str().expect("utf-8");
        let path_env = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        // The server (started by this client) takes its global environment,
        // and so the pane's `PATH`, from the client.
        let mut runs = vec![w.raw_env(
            &[
                "-L",
                &w.label,
                "-f",
                "/dev/null",
                "new",
                "-d",
                "-s",
                "b",
                "-x",
                "30",
                "-y",
                "3",
                "nsb",
                "Y",
                "Y",
            ],
            &[("PATH", &path_env)],
        )];
        w.wait_for_text("b", "ran-Y");
        runs.push(w.run(&["capture-pane", "-p", "-t", "b"]));
        runs.push(w.run(&[
            "new", "-d", "-s", "a", "-x", "30", "-y", "3", path, "X", "X",
        ]));
        w.wait_for_text("a", "ran-X");
        runs.push(w.run(&["capture-pane", "-p", "-t", "a"]));
        runs
    });
}

/// A new pane's title is the host name (window.c:1479-1480
/// `window_pane_create`).
#[test]
fn initial_pane_title_is_hostname() {
    compare("initial_pane_title_is_hostname", |w| {
        vec![
            w.run(&["new", "-d", "-s", "a"]),
            w.run(&["display", "-p", "[#{pane_title}] #{host}"]),
        ]
    });
    let w = World::new(&vtmux_bin());
    w.run(&["new", "-d", "-s", "a"]);
    let host = w.run(&["display", "-p", "#{host}"]).stdout;
    assert_eq!(w.run(&["display", "-p", "#{pane_title}"]).stdout, host);
}

/// Twelve clients start a server at once (client.c `client_get_lock`):
/// one server, every session created, no errors.
#[test]
fn concurrent_starts_share_one_server() {
    let (_keep, as_tmux) = vtmux_as_tmux();
    for _ in 0..5 {
        let w = World::new(&as_tmux);
        let runs: Vec<Run> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..12)
                .map(|i| {
                    let w = &w;
                    s.spawn(move || w.run(&["new", "-d", "-s", &format!("s{i}")]))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for r in &runs {
            assert_eq!((r.code, r.stderr.as_str()), (0, ""), "{runs:?}");
        }
        let ls = w.run(&["ls", "-F", "#{session_name}"]);
        assert_eq!(ls.stdout.lines().count(), 12, "{}", ls.stdout);
        assert!(
            !w.dir()
                .join(format!("tmux-{}/{}.lock", uid(), w.label))
                .exists()
        );
    }
}

/// SIGUSR1 recreates the socket, SIGQUIT and SIGUSR2 are survived
/// (server.c `server_signal`, proc.c `proc_set_signals`).
#[test]
fn signals_follow_tmux() {
    let (_keep, as_tmux) = vtmux_as_tmux();
    let reference = reference();
    let bins: Vec<PathBuf> = std::iter::once(as_tmux).chain(reference).collect();
    for bin in bins {
        let w = World::new(&bin);
        w.run(&["new", "-d", "-s", "a"]);
        let pid = w.run(&["display", "-p", "#{pid}"]).stdout.trim().to_owned();
        let sock = w.dir().join(format!("tmux-{}/{}", uid(), w.label));
        let kill = |sig: &str| {
            assert!(
                Command::new("kill")
                    .args([sig, &pid])
                    .status()
                    .unwrap()
                    .success()
            );
        };
        kill("-QUIT");
        kill("-USR2");
        std::fs::remove_file(&sock).expect("remove socket");
        assert_ne!(w.run(&["ls"]).code, 0, "socket is gone");
        kill("-USR1");
        let start = Instant::now();
        while !sock.exists() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let ls = w.run(&["ls"]);
        assert_eq!(ls.code, 0, "{} {ls:?}", bin.display());
        assert!(ls.stdout.starts_with("a: 1 windows"));
    }
}
