//! CLI scenarios run against `target/debug/vtmux` and the reference tmux
//! 3.8, compared after normalisation (socket dir, pid, time).
//!
//! Isolation: `TMUX` and `TMUX_PANE` cleared, a private `TMUX_TMPDIR` per
//! scenario, an explicit `-L` per binary, `-f /dev/null`, SHELL=/bin/sh.
//! The reference is `$VTMUX_REFERENCE_TMUX` or the `cargo xtask ref-tmux`
//! cache; without it each test prints why and checks vtmux alone against
//! the recorded expectations embedded below.

mod common;

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
            .env_remove("EDITOR")
            .env_remove("VISUAL")
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

    /// Start `-L label -f /dev/null args...` in the background.
    fn spawn(&self, args: &[&str]) -> std::process::Child {
        Command::new(&self.bin)
            .args(["-L", &self.label, "-f", "/dev/null"])
            .args(args)
            .current_dir(self.dir())
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env("TMUX_TMPDIR", self.dir())
            .env("SHELL", "/bin/sh")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn binary")
    }

    /// Poll `wait-for -l args` (max 5 s) until it lists `want`, with each
    /// `client-PID` of `pids` written `client-N` (N its index).
    fn waiters(&self, args: &[&str], pids: &[u32], want: &str) -> Run {
        let mut full = vec!["wait-for", "-l"];
        full.extend_from_slice(args);
        let start = Instant::now();
        loop {
            let mut r = self.run(&full);
            for (i, pid) in pids.iter().enumerate() {
                r.stdout = r
                    .stdout
                    .replace(&format!("client-{pid}\n"), &format!("client-{i}\n"));
            }
            if r.stdout == want || start.elapsed() > Duration::from_secs(5) {
                return r;
            }
            std::thread::sleep(Duration::from_millis(20));
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

    /// The pane's `stty size` must come to agree with `#{pane_height}
    /// #{pane_width}`: send Enter to a `while read` loop running `stty
    /// size` until its last line is that size (max 5 s). tmux sends
    /// TIOCSWINSZ from a 250 ms timer (server-client.c
    /// `server_client_check_pane_resize`), so the first answer may be old.
    /// Returns the size seen.
    fn pty_size_agrees(&self, target: &str) -> String {
        let want = self
            .run(&[
                "display",
                "-p",
                "-t",
                target,
                "#{pane_height} #{pane_width}",
            ])
            .stdout
            .trim()
            .to_owned();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            self.run(&["send-keys", "-t", target, "Enter"]);
            std::thread::sleep(Duration::from_millis(100));
            let screen = self.run(&["capture-pane", "-p", "-t", target]).stdout;
            if screen.lines().rev().find(|l| !l.trim().is_empty()) == Some(want.as_str()) {
                return want;
            }
        }
        panic!(
            "stty size in {target} never became {want:?} ({})",
            self.bin.display()
        );
    }

    /// Poll until `target` is on the alternate screen (max 5 s).
    fn wait_alternate(&self, target: &str) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if self
                .run(&["display", "-p", "-t", target, "#{alternate_on}"])
                .stdout
                == "1\n"
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "{target} never entered the alternate screen ({})",
            self.bin.display()
        );
    }

    /// Poll until `display -p -t target '#{window_name}'` is `want` (max
    /// 5 s): automatic rename settles only after the pane has printed
    /// (names.c `check_window_name` waits for `PANE_CHANGED`).
    fn wait_window_name(&self, target: &str, want: &str) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if self
                .run(&["display", "-p", "-t", target, "#{window_name}"])
                .stdout
                .trim_end()
                == want
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "window {target} never got named {want:?} ({})",
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
        let pid = common::parse_pid(&self.run(&["display", "-p", "#{pid}"]).stdout);
        let _ = self.run(&["kill-server"]);
        common::reap(pid);
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
        // After Enter, cat echoes "hi" back after the typed "xy"; wait for
        // the echo too, or the capture races it on either binary.
        w.wait_for_text("a", "xyhi");
        runs.push(w.run(&["capture-pane", "-p", "-t", "a"]));
        runs
    });
}

/// Copy mode's format variables (window-copy.c `window_copy_formats`,
/// `send-keys -FX search-* -- FMT` expands FMT for the pane, as the default
/// vi `*` and `#` bindings rely on (`window_copy_expand_search_string` reads
/// the command's own arguments, after its `--`; found in `review_copy_search`).
#[test]
fn send_keys_format_search_string() {
    compare("send_keys_format_search_string", |w| {
        let mut runs = vec![w.run(&[
            "new",
            "-d",
            "-s",
            "a",
            "-x",
            "40",
            "-y",
            "5",
            "sh",
            "-c",
            "printf 'alpha beta alpha beta\\n'; exec cat",
        ])];
        w.wait_for_text("a", "beta");
        for search in [
            &[
                "send-keys",
                "-t",
                "a",
                "-FX",
                "search-forward",
                "--",
                "#{copy_cursor_word}",
            ][..],
            &[
                "send-keys",
                "-t",
                "a",
                "-FX",
                "search-forward",
                "#{copy_cursor_word}",
            ][..],
            &[
                "send-keys",
                "-t",
                "a",
                "-FX",
                "search-backward",
                "--",
                "#{copy_cursor_word}",
            ][..],
        ] {
            runs.push(w.run(&["copy-mode", "-t", "a"]));
            runs.push(w.run(&["send-keys", "-t", "a", "-X", "history-top"]));
            runs.push(w.run(search));
            runs.push(w.run(&[
                "display",
                "-p",
                "-t",
                "a",
                "[#{pane_search_string}] #{copy_cursor_x}",
            ]));
            runs.push(w.run(&["send-keys", "-t", "a", "-X", "cancel"]));
        }
        runs
    });
}

/// format.c `format_cb_pane_in_mode`/`pane_mode`), `send-keys -X` with a
/// repeat count, mode keys through `send-keys`, and the errors outside a
/// mode (cmd-send-keys.c:191-204), emacs and vi keys.
#[test]
fn copy_mode_formats() {
    const F: &str = "[#{pane_in_mode}] [#{pane_mode}] [#{scroll_position}] \
        [#{copy_cursor_x},#{copy_cursor_y}] [#{copy_cursor_line}] [#{copy_cursor_word}] \
        [#{copy_position}/#{copy_position_limit}] [#{copy_line_numbers}] \
        [#{selection_present}] [#{selection_active}] [#{selection_mode}] \
        [#{rectangle_toggle}] [#{search_present}] [#{search_count}] [#{refresh_active}] \
        #{cursor_x},#{cursor_y} #{history_size}";
    compare("copy_mode_formats", |w| {
        let mut runs = vec![w.run(&[
            "new",
            "-d",
            "-s",
            "a",
            "-x",
            "30",
            "-y",
            "6",
            "sh",
            "-c",
            "for i in 1 2 3 4 5 6 7 8 9; do echo line$i; done; echo 'foo bar-baz'; exec cat",
        ])];
        w.wait_for_text("a", "bar-baz");
        let mut step = |args: &[&str]| {
            runs.push(w.run(args));
            runs.push(w.run(&["display", "-p", "-t", "a", F]));
        };
        step(&["send-keys", "-t", "a", "-X", "cancel"]);
        step(&["send-keys", "-t", "a", "-X", "-N", "2", "cancel"]);
        step(&["copy-mode", "-t", "a"]);
        step(&["send-keys", "-t", "a", "-X", "cursor-up"]);
        step(&["send-keys", "-t", "a", "-X", "-N", "5", "cursor-right"]);
        step(&["send-keys", "-t", "a", "-X", "-N", "3", "cursor-right"]);
        step(&["send-keys", "-t", "a", "-X", "page-up"]);
        step(&["send-keys", "-t", "a", "-X", "-N", "0", "cursor-up"]);
        step(&["send-keys", "-t", "a", "-N", "2", "Up"]);
        step(&["send-keys", "-t", "a", "-X", "history-top"]);
        step(&["send-keys", "-t", "a", "-X", "goto-line", "2"]);
        step(&["send-keys", "-t", "a", "-X", "nosuch"]);
        step(&["send-keys", "-t", "a", "-X"]);
        step(&["send-keys", "-t", "a", "q"]);
        step(&["set", "-g", "mode-keys", "vi"]);
        step(&["copy-mode", "-u", "-t", "a"]);
        step(&["send-keys", "-t", "a", "k", "$"]);
        step(&["send-keys", "-t", "a", "-X", "line-numbers-off"]);
        step(&["copy-mode", "-t", "a"]);
        step(&["copy-mode", "-q", "-t", "a"]);
        runs
    });
}

/// The selection formats (`window_copy_formats`, window-copy.c:1136-1158)
/// through begin/stop/clear-selection, other-end, rectangle-toggle,
/// selection-mode, select-word and select-line, and the buffers the copy
/// commands make (prefixes, -P, append).
#[test]
fn copy_selection_formats() {
    const F: &str = "[#{selection_present}#{selection_active}] [#{selection_mode}] \
        [#{selection_start_x},#{selection_start_y}-#{selection_end_x},#{selection_end_y}] \
        [#{rectangle_toggle}] [#{copy_cursor_x},#{copy_cursor_y}] [#{scroll_position}]";
    compare("copy_selection_formats", |w| {
        let mut runs = vec![w.run(&[
            "new",
            "-d",
            "-s",
            "a",
            "-x",
            "30",
            "-y",
            "6",
            "sh",
            "-c",
            "for i in 1 2 3 4 5 6; do echo line$i; done; echo 'foo bar-baz qux'; echo 'last'; exec cat",
        ])];
        w.wait_for_text("a", "last");
        let mut step = |args: &[&str]| {
            runs.push(w.run(args));
            runs.push(w.run(&["display", "-p", "-t", "a", F]));
        };
        step(&["copy-mode", "-t", "a"]);
        step(&["send-keys", "-t", "a", "-X", "-N", "2", "cursor-up"]);
        step(&["send-keys", "-t", "a", "-X", "begin-selection"]);
        step(&["send-keys", "-t", "a", "-X", "-N", "6", "cursor-right"]);
        step(&["send-keys", "-t", "a", "-X", "other-end"]);
        step(&["send-keys", "-t", "a", "-X", "cursor-up"]);
        step(&["send-keys", "-t", "a", "-X", "rectangle-toggle"]);
        step(&["send-keys", "-t", "a", "-X", "stop-selection"]);
        step(&["send-keys", "-t", "a", "-X", "cursor-down"]);
        step(&[
            "send-keys",
            "-t",
            "a",
            "-X",
            "copy-selection-no-clear",
            "pre",
        ]);
        step(&["send-keys", "-t", "a", "-X", "clear-selection"]);
        step(&["send-keys", "-t", "a", "-X", "rectangle-off"]);
        step(&["send-keys", "-t", "a", "-X", "select-word"]);
        step(&["send-keys", "-t", "a", "-X", "selection-mode", "line"]);
        step(&["send-keys", "-t", "a", "-X", "select-line"]);
        step(&["send-keys", "-t", "a", "-X", "cursor-up"]);
        step(&["send-keys", "-t", "a", "-X", "copy-selection"]);
        step(&["send-keys", "-t", "a", "-X", "selection-mode", "W"]);
        step(&["send-keys", "-t", "a", "-X", "selection-mode", "bogus"]);
        step(&["send-keys", "-t", "a", "-X", "selection-mode", "a", "b"]);
        step(&[
            "send-keys",
            "-t",
            "a",
            "-X",
            "-N",
            "2",
            "copy-line",
            "x#{pane_id}-",
        ]);
        step(&["send-keys", "-t", "a", "-X", "copy-end-of-line", "-P"]);
        step(&["send-keys", "-t", "a", "-X", "begin-selection"]);
        step(&["send-keys", "-t", "a", "-X", "next-word-end"]);
        step(&["send-keys", "-t", "a", "-X", "append-selection"]);
        step(&["send-keys", "-t", "a", "-X", "copy-selection-and-cancel"]);
        runs.push(w.run(&["list-buffers", "-F", "#{buffer_name}=#{buffer_sample}"]));
        runs
    });
}

/// The copy mode pipe commands (window-copy.c:2660-2720, 1380-1560): the
/// selection to a job's stdin, `copy-command` when no command is given,
/// the buffer prefix and `-P`, the line variants with a repeat count, the
/// formats in the command expanded before the cursor moves, and what
/// each leaves of the selection and the mode. Reference: tmux 3.8.
#[test]
#[allow(clippy::too_many_lines)] // one transcript
fn copy_pipe_commands() {
    compare("copy_pipe_commands", |w| {
        let d = w.dir().to_string_lossy().into_owned();
        // The job writes `name.t` and renames it, so a reader sees all of it.
        let sink = |name: &str| format!("cat > {d}/{name}.t && mv {d}/{name}.t {d}/{name}");
        let read = |name: &str| -> Run {
            let path = w.dir().join(name);
            let start = Instant::now();
            while !path.exists() && start.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(20));
            }
            let text = std::fs::read(&path).map_or_else(
                |_| "MISSING".into(),
                |b| String::from_utf8_lossy(&b).into_owned(),
            );
            Run {
                stdout: w.normalise(&text),
                stderr: String::new(),
                code: 0,
            }
        };
        let mut runs = vec![w.run(&[
            "new",
            "-d",
            "-s",
            "a",
            "-x",
            "30",
            "-y",
            "6",
            "sh",
            "-c",
            "printf 'alpha beta\\ngamma delta\\nthird line\\n'; echo last; exec cat",
        ])];
        w.wait_for_text("a", "last");
        let state = |w: &World| {
            w.run(&[
                "display",
                "-p",
                "-t",
                "a",
                "[#{pane_in_mode}] [#{selection_present}] [#{copy_cursor_x},#{copy_cursor_y}]",
            ])
        };
        let x = |w: &World, args: &[&str]| {
            let mut full = vec!["send-keys", "-t", "a", "-X"];
            full.extend_from_slice(args);
            w.run(&full)
        };
        let select = |w: &World, runs: &mut Vec<Run>| {
            runs.push(w.run(&["copy-mode", "-t", "a"]));
            runs.push(x(w, &["history-top"]));
            runs.push(x(w, &["start-of-line"]));
            runs.push(x(w, &["begin-selection"]));
            runs.push(x(w, &["-N", "4", "cursor-right"]));
        };
        // Each of the three keeping the mode, with and without a prefix.
        select(w, &mut runs);
        runs.push(x(w, &["copy-pipe", &sink("o1")]));
        runs.push(read("o1"));
        runs.push(state(w));
        runs.push(x(w, &["begin-selection"]));
        runs.push(x(w, &["cursor-right"]));
        runs.push(x(w, &["copy-pipe-no-clear", &sink("o2"), "pre#{pane_id}-"]));
        runs.push(read("o2"));
        runs.push(state(w));
        runs.push(x(w, &["pipe-no-clear", &sink("o3")]));
        runs.push(read("o3"));
        runs.push(state(w));
        runs.push(x(w, &["pipe", &sink("o4")]));
        runs.push(read("o4"));
        runs.push(state(w));
        runs.push(x(w, &["cancel"]));
        // -P: no buffer; -C: no clipboard; -and-cancel leaves the mode.
        select(w, &mut runs);
        runs.push(x(w, &["copy-pipe-and-cancel", "-P", &sink("o5")]));
        runs.push(read("o5"));
        runs.push(state(w));
        select(w, &mut runs);
        runs.push(x(w, &["pipe-and-cancel", &sink("o6")]));
        runs.push(read("o6"));
        runs.push(state(w));
        runs.push(w.run(&["list-buffers", "-F", "#{buffer_name}=#{buffer_sample}"]));
        // The line variants, one and two lines, from the middle of a line.
        runs.push(w.run(&["copy-mode", "-t", "a"]));
        runs.push(x(w, &["history-top"]));
        runs.push(x(w, &["cursor-down"]));
        runs.push(x(w, &["-N", "3", "cursor-right"]));
        runs.push(x(w, &["copy-pipe-line", &sink("o7"), "ln"]));
        runs.push(read("o7"));
        runs.push(state(w));
        runs.push(x(w, &["-N", "2", "copy-pipe-end-of-line", &sink("o8")]));
        runs.push(read("o8"));
        runs.push(state(w));
        runs.push(x(w, &["-N", "2", "copy-pipe-line-and-cancel", &sink("o9")]));
        runs.push(read("o9"));
        runs.push(state(w));
        runs.push(w.run(&["copy-mode", "-t", "a"]));
        runs.push(x(w, &["history-top"]));
        runs.push(x(w, &["copy-pipe-end-of-line-and-cancel", &sink("o10")]));
        runs.push(read("o10"));
        runs.push(state(w));
        runs.push(w.run(&["list-buffers", "-F", "#{buffer_name}=#{buffer_sample}"]));
        // No command: `copy-command`; none of those: nothing runs.
        select(w, &mut runs);
        runs.push(x(w, &["copy-pipe"]));
        runs.push(w.run(&["set-option", "-s", "copy-command", &sink("o11")]));
        runs.push(x(w, &["begin-selection"]));
        runs.push(x(w, &["-N", "2", "cursor-right"]));
        runs.push(x(w, &["copy-pipe-no-clear"]));
        runs.push(read("o11"));
        std::fs::remove_file(w.dir().join("o11")).ok();
        runs.push(x(w, &["pipe", ""]));
        runs.push(read("o11"));
        runs.push(x(w, &["cancel"]));
        // Formats in the command see the state before the line moves the
        // cursor; the job's directory and environment are the server's.
        runs.push(w.run(&["set-option", "-s", "copy-command", ""]));
        runs.push(w.run(&["copy-mode", "-t", "a"]));
        runs.push(x(w, &["history-top"]));
        runs.push(x(
            w,
            &[
                "copy-pipe-line-and-cancel",
                &format!(
                    "cat > {d}/o12.t; pwd >> {d}/o12.t; echo $PWD >> {d}/o12.t; \
                     mv {d}/o12.t {d}/o12-#{{copy_cursor_y}}"
                ),
            ],
        ));
        runs.push(read("o12-0"));
        // No selection: the job still runs, with empty input.
        runs.push(w.run(&["copy-mode", "-t", "a"]));
        runs.push(x(w, &["copy-pipe-no-clear", &sink("o13")]));
        runs.push(read("o13"));
        runs.push(x(w, &["cancel"]));
        runs.push(w.run(&["list-buffers", "-F", "#{buffer_name}=#{buffer_sample}"]));
        runs
    });
}

/// `refresh-on` copies the pane's screen again by itself while output
/// arrives, `refresh-now` does it once, and neither happens without
/// them (window-copy.c:3040-3205): `#{pane_unseen_changes}`,
/// `#{refresh_active}`, the scroll position and the line under the
/// cursor. Reference: tmux 3.8.
#[test]
fn copy_refresh() {
    const F: &str = "[#{pane_unseen_changes}] [#{refresh_active}] [#{scroll_position}] \
                     [#{copy_cursor_y}] [#{copy_cursor_line}]";
    compare("copy_refresh", |w| {
        let d = w.dir().to_string_lossy().into_owned();
        // Two rounds of output, each started by its own `go` file.
        let script = format!(
            "echo first; for r in 1 2; do while [ ! -e {d}/go$r ]; do sleep 0.05; done; \
             for i in 1 2 3; do echo r$r-$i; sleep 0.05; done; touch {d}/done$r; done; exec cat"
        );
        let mut runs = vec![w.run(&[
            "new", "-d", "-s", "a", "-x", "30", "-y", "5", "sh", "-c", &script,
        ])];
        w.wait_for_text("a", "first");
        let round = |w: &World, runs: &mut Vec<Run>, r: u8, on: bool| {
            runs.push(w.run(&["copy-mode", "-t", "a"]));
            if on {
                runs.push(w.run(&["send-keys", "-t", "a", "-X", "refresh-on"]));
            }
            runs.push(w.run(&["display", "-p", "-t", "a", F]));
            std::fs::write(format!("{d}/go{r}"), "").unwrap();
            let done = format!("{d}/done{r}");
            let start = Instant::now();
            while !Path::new(&done).exists() && start.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(20));
            }
            // The refresh timer is 50 ms; give it several.
            std::thread::sleep(Duration::from_millis(400));
            runs.push(w.run(&["display", "-p", "-t", "a", F]));
        };
        // Without a refresh the copy stays as it was; refresh-now takes it.
        round(w, &mut runs, 1, false);
        runs.push(w.run(&["send-keys", "-t", "a", "-X", "refresh-now"]));
        runs.push(w.run(&["display", "-p", "-t", "a", F]));
        runs.push(w.run(&["send-keys", "-t", "a", "-X", "cancel"]));
        // refresh-on takes it by itself.
        round(w, &mut runs, 2, true);
        runs.push(w.run(&["send-keys", "-t", "a", "-X", "history-top"]));
        runs.push(w.run(&["display", "-p", "-t", "a", F]));
        runs.push(w.run(&["send-keys", "-t", "a", "-X", "cancel"]));
        runs.push(w.run(&["copy-mode", "-t", "a"]));
        for c in [
            "refresh-on",
            "refresh-on",
            "refresh-toggle",
            "refresh-toggle",
            "refresh-off",
        ] {
            runs.push(w.run(&["send-keys", "-t", "a", "-X", c]));
            runs.push(w.run(&["display", "-p", "-t", "a", "#{refresh_active}"]));
        }
        runs.push(w.run(&["send-keys", "-t", "a", "-X", "cancel"]));
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

/// `detach-client` with no attached client: `CMD_CLIENT_TFLAG` fails first,
/// whatever `-a`, `-s`, `-P` or `-E` say (cmd-queue.c:604-609).
#[test]
fn detach_client_without_clients() {
    compare("detach_client_without_clients", |w| {
        vec![
            w.run(&["new", "-d", "-s", "a"]),
            w.run(&["detach-client"]),
            w.run(&["detach-client", "-a"]),
            w.run(&["detach-client", "-s", "a"]),
            w.run(&["detach-client", "-s", "nosuch"]),
            w.run(&["detach-client", "-P", "-E", "true"]),
            w.run(&["detach-client", "-t", "foo"]),
            w.run(&["detach-client", "-t", "foo:", "-a"]),
            w.run(&["ls"]),
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

/// `#{current_file}` and `#{d:current_file}` in commands parsed from a
/// file (cfg.c `load_cfg`: `cmdq_add_format(state, "current_file", ...)`,
/// merged by `cmdq_merge_formats`): empty outside a file, the inner file
/// inside a nested `source-file` (also with `-F`, whose path expands
/// `#{d:current_file}`) and the outer file again afterwards; commands
/// queued by `if-shell -F` and `run-shell -C` keep the file. Paths differ
/// between the two worlds, so the expectations are compared in the file
/// against the world's directory.
#[test]
fn source_file_current_file() {
    compare("source_file_current_file", |w| {
        let dir = w.dir().to_str().expect("utf-8").to_owned();
        let write = |name: &str, text: String| {
            std::fs::write(w.dir().join(name), text).expect("write");
        };
        write(
            "inner.conf",
            format!(
                "set -gF @in '#{{==:#{{current_file}},{dir}/inner.conf}}'\n\
                 set -gF @in_d '#{{==:#{{d:current_file}},{dir}}}'\n\
                 set -gF @in_b '#{{b:current_file}}'\n"
            ),
        );
        write(
            "outer.conf",
            format!(
                "set -gF @before '#{{==:#{{current_file}},{dir}/outer.conf}}'\n\
                 set -gF @d '#{{==:#{{d:current_file}},{dir}}}'\n\
                 source-file {dir}/inner.conf\n\
                 source-file -F '#{{d:current_file}}/inner.conf'\n\
                 set -gF @after '#{{==:#{{current_file}},{dir}/outer.conf}}'\n\
                 if-shell -F 1 \"set -gF @ifs '#{{==:#{{current_file}},{dir}/outer.conf}}'\"\n\
                 run-shell -C \"set -gF @rsc '#{{==:#{{current_file}},{dir}/outer.conf}}'\"\n\
                 %if '#{{==:#{{current_file}},}}'\n\
                 set -g @cond top\n\
                 %endif\n"
            ),
        );
        let outer = format!("{dir}/outer.conf");
        vec![
            w.run(&["new", "-d", "-s", "a"]),
            w.run(&["source", &outer]),
            w.run(&[
                "display",
                "-p",
                "#{@before} #{@d} #{@in} #{@in_d} #{@in_b} #{@after} #{@ifs} #{@rsc} #{@cond} [#{current_file}]",
            ]),
        ]
    });
}

/// `%if` in config files (cmd-parse.y `condition`, `condition1`,
/// `expanded`): the whole file is parsed before anything runs, so a `set`
/// is not seen by a later `%if`; conditions are formats over the parsing
/// client's current state (here the best session, `cmd_find_from_client`);
/// false branches are lexed but not built; a `%if` inside `{ }` is decided
/// when the file is parsed; a guard on a format that is empty takes the
/// else branch (`#{@unset}` here; `#{vtmux_version}` is that on tmux and
/// set on vtmux, D21, checked below).
#[test]
fn source_file_conditions() {
    compare("source_file_conditions", |w| {
        let write = |name: &str, text: &str| {
            let path = w.dir().join(name);
            std::fs::write(&path, text).expect("write");
            path.to_str().expect("utf-8").to_owned()
        };
        let good = write(
            "if.conf",
            "set -g @q 1\n\
             %if \"#{@q}\"\nset -g @r set\n%else\nset -g @r unset\n%endif\n\
             %if #{==:#{session_name},a}\n\
             %if 0\nnosuchcommand\n%elif 1\nset -g @n elif\n%else\nset -g @n else\n%endif\n\
             %endif\n\
             set -g @i x ; %if 1 set -g @i inl %endif\n\
             %if \"#{@unset}\"\nset -g @v set\n%else\nset -g @v empty\n%endif\n\
             %if \"#(echo 1)\"\nset -g @j job\n%endif\n\
             bind -T t x { %if 1\ndisplay -p a\n%else\ndisplay -p b\n%endif\n}\n",
        );
        let bad_quote = write("q.conf", "set -g @b 1\n%if 0\ndisplay \"abc\n%endif\n");
        let unterminated = write("u.conf", "%if 1\nset -g @u 1\n");
        vec![
            w.run(&["new", "-d", "-s", "a"]),
            w.run(&["source", "-v", &good]),
            w.run(&["display", "-p", "#{@q} #{@r} #{@n} #{@i} #{@v} [#{@j}]"]),
            w.run(&["list-keys", "-T", "t"]),
            w.run(&["source", "-nv", &good]),
            w.run(&["source", &bad_quote]),
            w.run(&["source", &unterminated]),
            w.run(&["display", "-p", "[#{@b}] [#{@u}]"]),
        ]
    });
    // The D23 guard: the same file takes the vtmux branch only on vtmux.
    let w = World::new(&vtmux_bin());
    let conf = w.dir().join("guard.conf");
    std::fs::write(
        &conf,
        "%if \"#{vtmux_version}\"\nset -g @v vtmux\n%else\nset -g @v tmux\n%endif\n",
    )
    .expect("write");
    w.run(&["new", "-d", "-s", "a"]);
    let r = w.run(&["source", conf.to_str().expect("utf-8")]);
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert_eq!(w.run(&["display", "-p", "#{@v}"]).stdout, "vtmux\n");
}

/// A startup config runs before the first command, without a client:
/// `new` in it is detached, errors are kept for an attached session and
/// never reach a command client (cfg.c `start_cfg`, `cfg_show_causes`).
#[test]
fn startup_config() {
    compare("startup_config", |w| {
        let conf = w.dir().join("start.conf");
        // `new -d` with a long-lived pane: without -d, new tries to attach a
        // tty-less client and fails, so the server may exit (exit-empty)
        // before `ls` runs, on tmux and vtmux alike, which made this flaky.
        std::fs::write(&conf, "new -d -s test 'sleep 30'\ndisplay hello\nnosuch\n").expect("write");
        let c = conf.to_str().expect("utf-8");
        vec![
            w.raw(&["-L", &w.label, "-f", c, "start"]),
            w.run(&["ls", "-F", "#{session_name}"]),
        ]
    });
}

/// The startup config is parsed for the client that started the server
/// (cfg.c:81-90, `pi.c`), attached or not, so `%if` sees `client_name` and
/// `client_pid`; its commands still run with no client. A non-UTF-8 word in
/// a dropped branch does not reject the file (cmd-parse.y builds only the
/// branch taken).
#[test]
fn startup_config_conditions() {
    compare("startup_config_conditions", |w| {
        let conf = w.dir().join("cond.conf");
        std::fs::write(
            &conf,
            "%if \"#{client_name}\"\nset -g @cn has\n%else\nset -g @cn none\n%endif\n\
             %if \"#{client_pid}\"\nset -g @cp has\n%else\nset -g @cp none\n%endif\n\
             %if \"#{session_name}\"\nset -g @sn has\n%else\nset -g @sn none\n%endif\n\
             %if 0\nset -g @a \"\\377\"\n%endif\nset -g @b 1\n",
        )
        .expect("write");
        let c = conf.to_str().expect("utf-8");
        vec![
            w.raw(&["-L", &w.label, "-f", c, "new", "-d", "-s", "s"]),
            w.run(&["show", "-g", "@cn"]),
            w.run(&["show", "-g", "@cp"]),
            w.run(&["show", "-g", "@sn"]),
            w.run(&["show", "-g", "@b"]),
            w.run(&["show", "-g", "@a"]),
        ]
    });
}

/// `source-file` with a non-UTF-8 word: dropped in a false branch, an
/// error only where it is built.
#[test]
fn source_file_non_utf8_in_false_branch() {
    compare("source_file_non_utf8_in_false_branch", |w| {
        let write = |name: &str, text: &str| {
            let path = w.dir().join(name);
            std::fs::write(&path, text).expect("write");
            path.to_str().expect("utf-8").to_owned()
        };
        let dropped = write(
            "d.conf",
            "%if 0\nset -g @a \"\\377\"\n%endif\nset -g @b 1\n",
        );
        vec![
            w.run(&["new", "-d", "-s", "a"]),
            w.run(&["source", "-v", &dropped]),
            w.run(&["show", "-g", "@b"]),
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

/// Reference: isolated `tmux new -d -s a cat`, then the set/show/display
/// invocations below. Missing registration, scope storage or format lookup
/// breaks this round trip at the CLI boundary.
#[test]
fn options_user_scopes() {
    compare("options_user_scopes", |w| {
        vec![
            w.run(&["new", "-d", "-s", "a", "cat"]),
            w.run(&["set-option", "-s", "@x", "server"]),
            w.run(&["set", "-g", "@x", "global session"]),
            w.run(&["setw", "-g", "@x", "global window"]),
            w.run(&["set", "-t", "a", "@x", "session"]),
            w.run(&["set", "-w", "-t", "@0", "@x", "window"]),
            w.run(&["set", "-p", "-t", "%0", "@x", "pane"]),
            w.run(&["show", "-s", "@x"]),
            w.run(&["show", "-g", "@x"]),
            w.run(&["showw", "-g", "@x"]),
            w.run(&["show", "-t", "a", "@x"]),
            w.run(&["show", "-w", "-t", "@0", "@x"]),
            w.run(&["show", "-p", "-t", "%0", "@x"]),
            w.run(&["display", "-p", "#{@x}"]),
            w.run(&["set", "-su", "@x"]),
            w.run(&["display", "-p", "#{@x}"]),
            w.run(&["set", "-pu", "-t", "%0", "@x"]),
            w.run(&["display", "-p", "#{@x}"]),
            w.run(&["set", "-wu", "-t", "@0", "@x"]),
            w.run(&["show", "-pA", "-t", "%0", "@x"]),
            w.run(&["display", "-p", "#{@x}"]),
        ]
    });
}

/// Re-run the core's reference probes at the server boundary. Generating
/// invocations: crates/vtmux-core/tests/options_cases.sh (isolated new -d
/// -s s, then each 0x1f-delimited argv), set-hook and show-hooks included.
#[test]
fn options_core_probes_over_cli() {
    let mut checked = 0;
    for case in include_str!("../../vtmux-core/src/options/golden/cases.txt")
        .split("### ")
        .skip(1)
    {
        let name = case.lines().next().unwrap();
        let commands: Vec<Vec<&str>> = case
            .lines()
            .filter_map(|line| line.strip_prefix("$ "))
            .map(|line| line.split('\x1f').collect())
            .collect();
        compare(name, |w| {
            let mut runs = vec![w.run(&["new", "-d", "-s", "s", "cat"])];
            runs.extend(commands.iter().map(|argv| {
                let mut r = w.run(argv);
                if name == "names-malformed" {
                    r.stderr = unparsable_option_error(&r.stderr);
                }
                r
            }));
            runs
        });
        checked += 1;
    }
    assert_eq!(checked, 52);
}

/// For a name `options_parse` rejects, `options_match` returns without
/// setting `*ambiguous` and set/show read it anyway (options.c:784-799,
/// cmd-show-options.c:125-133), so the word depends on how the reference
/// was compiled: `invalid` for `show` on this host's build, `ambiguous` on
/// CI's Ubuntu build. Only the word is unspecified; the message and exit
/// still compare. vtmux pins the local build (options/table.rs
/// `MatchError::Unparsable`).
fn unparsable_option_error(stderr: &str) -> String {
    stderr.replace("ambiguous option: ", "invalid option: ")
}

/// spawn.c `spawn_pane` reads default-shell and history-limit for each new
/// pane; server.c `server_loop` keeps an empty server with exit-empty off.
#[test]
fn options_spawn_and_exit_empty() {
    compare("options_spawn_and_exit_empty", |w| {
        use std::os::unix::fs::PermissionsExt;
        let shell = w.dir().join("shell");
        std::fs::write(
            &shell,
            "#!/bin/sh\nprintf 'configured-shell\\n'\nexec /bin/sh \"$@\"\n",
        )
        .expect("write shell");
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let mut runs = vec![
            w.run(&["new", "-d", "-s", "a", "cat"]),
            w.run(&["set", "-g", "default-shell", shell.to_str().expect("utf-8")]),
            w.run(&["set", "-g", "history-limit", "3"]),
            w.run(&[
                "new",
                "-d",
                "-s",
                "b",
                "-x",
                "30",
                "-y",
                "3",
                "printf '1\\n2\\n3\\n4\\n5\\n6\\n7\\n8\\n9\\n10\\n'; exec cat",
            ]),
        ];
        w.wait_for_text("b", "10");
        runs.push(w.run(&["display", "-p", "-t", "b", "#{history_size}"]));
        runs.push(w.run(&["new", "-d", "-s", "c", "exec cat"]));
        w.wait_for_text("c", "configured-shell");
        runs.push(w.run(&["capture-pane", "-p", "-t", "c"]));
        runs.push(w.run(&["kill-session", "-t", "c"]));
        runs.push(w.run(&["set", "-s", "exit-empty", "off"]));
        runs.push(w.run(&["kill-session", "-t", "a"]));
        runs.push(w.run(&["kill-session", "-t", "b"]));
        runs.push(w.run(&["show", "-sv", "exit-empty"]));
        runs.push(w.run(&["show", "-gv", "default-shell"]));
        runs.push(w.run(&["set", "-s", "exit-empty", "on"]));
        w.wait_gone();
        runs.push(w.run(&["ls"]));
        runs
    });
}

/// History parity with Ghostty's line limit (history-memory spec,
/// criterion 2): Ghostty keeps at most a page more than `history-limit`,
/// whatever the styling, and `#{history_size}` and `capture-pane -S -`
/// stay tmux's (`grid_collect_history`, grid.c:449-474). A styled 80
/// column pane (40 truecolour runs per line, where a byte budget kept
/// 9664 rows to tmux's 9979), a 500 column pane, and reflow of the wide
/// pane's history when it narrows and widens again (grid.c `grid_reflow`
/// keeps every line, so `history_size` passes the limit). The live `history-limit` is pinned by the
/// `history_limit_live*` oracle scenarios.
#[test]
fn history_limit_retention() {
    compare("history_limit_retention", |w| {
        let styled = "awk 'BEGIN { for (i = 0; i < 12000; i++) { s = sprintf(\"%05d\", i); \
            for (j = 0; j < 40; j++) s = s sprintf(\"\\033[38;2;%d;%d;%dm%c\", i % 256, j, (i * 7) % 256, 65 + j % 26); \
            print s \"\\033[m\" } }'; echo DONE; exec cat";
        let wide = "awk 'BEGIN { for (i = 0; i < 12000; i++) { s = sprintf(\"%05d\", i); \
            while (length(s) < 450) s = s \" \" i; print s } }'; echo DONE; exec cat";
        let mut runs = vec![
            w.run(&["new", "-d", "-s", "a", "cat"]),
            w.run(&["set", "-g", "history-limit", "10000"]),
            w.run(&["new", "-d", "-s", "s", "-x", "80", "-y", "24", styled]),
            w.run(&["new", "-d", "-s", "w", "-x", "500", "-y", "24", wide]),
        ];
        w.wait_for_text("s", "DONE");
        w.wait_for_text("w", "DONE");
        // DONE on screen can come before the server has fed every byte to
        // the pane under load: wait until history_size stops changing.
        for target in ["s", "w"] {
            let mut last = String::new();
            let start = Instant::now();
            loop {
                let now = w
                    .run(&["display", "-p", "-t", target, "#{history_size}"])
                    .stdout;
                if now == last || start.elapsed() > Duration::from_secs(10) {
                    break;
                }
                last = now;
                std::thread::sleep(Duration::from_millis(300));
            }
        }
        let state = ["display", "-p", "-t", "", "#{history_size} #{pane_width}"];
        for (target, size) in [
            ("s", None),
            ("w", None),
            ("w", Some("100")),
            ("w", Some("500")),
        ] {
            if let Some(cols) = size {
                runs.push(w.run(&["resize-window", "-t", target, "-x", cols]));
            }
            let mut display = state;
            display[3] = target;
            runs.push(w.run(&display));
            runs.push(w.run(&["capture-pane", "-p", "-S", "-", "-t", target]));
        }
        runs
    });
}

/// A resize while the alternate screen is up leaves the primary history
/// alone in tmux (window.c `window_pane_resize` resizes without reflow
/// while `saved_grid` is set) and reflows it once when the alternate
/// screen goes (screen.c `screen_alternate_off`). Ghostty reflows the
/// primary at every resize: narrowing must not let the line limit prune
/// the rows it split. A split and kill (80 -> 40 -> 80 columns) and a
/// resize-window to 40x20 then 90x30, on plain and truecolour output.
#[test]
fn history_resize_under_the_alternate_screen() {
    compare("history_resize_under_the_alternate_screen", |w| {
        let script = |styled: bool| {
            let cell = if styled {
                "sprintf(\"\\033[38;2;%d;%d;%dm%c\", i % 256, j, (i * 7) % 256, 65 + j % 26)"
            } else {
                "sprintf(\"%c\", 97 + (i + j) % 26)"
            };
            format!(
                "awk 'BEGIN {{ for (i = 0; i < 5000; i++) {{ s = sprintf(\"%05d \", i); \
                 for (j = 0; j < 64; j++) s = s {cell}; print s \"\\033[m\" }} }}'; \
                 echo DONE; read x; printf '\\033[?1049h'; echo ALT; read x; \
                 printf '\\033[?1049l'; echo BACK; exec cat"
            )
        };
        let mut runs = vec![
            w.run(&["new", "-d", "-s", "k", "cat"]),
            w.run(&["set", "-g", "history-limit", "2000"]),
        ];
        let state = |t: &str| {
            [
                "display",
                "-p",
                "-t",
                t,
                "#{history_size} #{pane_width}x#{pane_height}",
            ]
            .map(String::from)
        };
        for (name, styled, resize) in [("p", false, false), ("q", false, true), ("s", true, true)] {
            runs.push(w.run(&[
                "new",
                "-d",
                "-s",
                name,
                "-x",
                "80",
                "-y",
                "24",
                &script(styled),
            ]));
            w.wait_for_text(name, "DONE");
            runs.push(w.run(&["send-keys", "-t", name, "Enter"]));
            w.wait_for_text(name, "ALT");
            if resize {
                runs.push(w.run(&["resize-window", "-t", name, "-x", "40", "-y", "20"]));
                runs.push(w.run(&["resize-window", "-t", name, "-x", "90", "-y", "30"]));
            } else {
                runs.push(w.run(&["split-window", "-h", "-d", "-t", name, "cat"]));
                runs.push(w.run(&["kill-pane", "-t", &format!("{name}.1")]));
            }
            let s = state(name);
            runs.push(w.run(&s.each_ref().map(String::as_str)));
            runs.push(w.run(&["send-keys", "-t", name, "Enter"]));
            w.wait_for_text(name, "BACK");
            runs.push(w.run(&s.each_ref().map(String::as_str)));
            runs.push(w.run(&["capture-pane", "-p", "-S", "-", "-t", name]));
        }
        runs
    });
}

/// spawn.c `spawn_pane` (379-390): a new pane with no command runs the
/// session's `default-command` as one `$SHELL -c` word; respawn-pane and
/// respawn-window without a command keep the pane's own. Scopes: session
/// value over global, `set -u` falls back, an empty value is a login shell,
/// and `default-shell` is the shell that runs it.
#[test]
fn default_command_spawns_panes() {
    compare("default_command_spawns_panes", |w| {
        use std::os::unix::fs::PermissionsExt;
        let shell = w.dir().join("shell");
        std::fs::write(
            &shell,
            "#!/bin/sh\nprintf 'configured-shell\\n'\nexec /bin/sh \"$@\"\n",
        )
        .expect("write shell");
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let list = |w: &World| {
            // The child execs the command after the fork: wait for the
            // listing to settle.
            let args = [
                "list-panes",
                "-a",
                "-F",
                "#{session_name}:#{window_index}.#{pane_index} [#{pane_start_command}] [#{pane_current_command}]",
            ];
            let mut last = w.run(&args);
            std::thread::sleep(Duration::from_millis(150));
            for _ in 0..40 {
                let now = w.run(&args);
                if now == last {
                    break;
                }
                last = now;
                std::thread::sleep(Duration::from_millis(100));
            }
            last
        };
        let mut runs = vec![
            w.run(&["new", "-d", "-x", "80", "-y", "24", "-s", "a", "cat"]),
            w.run(&["set", "-g", "default-command", "cat"]),
            w.run(&["split-window", "-d", "-t", "a"]),
            w.run(&["new-window", "-d", "-t", "a"]),
            w.run(&["new-session", "-d", "-s", "b"]),
            w.run(&["set", "-t", "a", "default-command", "sleep 99"]),
            w.run(&["new-window", "-d", "-t", "a"]),
            w.run(&["split-window", "-d", "-t", "b"]),
            list(w),
            // respawn without a command keeps the stored one.
            w.run(&["respawn-pane", "-k", "-t", "a:1.0"]),
            w.run(&["respawn-window", "-k", "-t", "a:2"]),
            w.run(&["set", "-g", "default-command", "exec cat"]),
            w.run(&["respawn-pane", "-k", "-t", "a:0.1"]),
            list(w),
            w.run(&["set", "-u", "-t", "a", "default-command"]),
            w.run(&["new-window", "-d", "-t", "a"]),
            w.run(&["set", "-g", "default-command", ""]),
            w.run(&["new-window", "-d", "-t", "b"]),
            w.run(&["set", "-g", "default-shell", shell.to_str().expect("utf-8")]),
            w.run(&["new-window", "-d", "-t", "b"]),
            w.run(&["set", "-g", "default-command", "exec cat"]),
            w.run(&["new-window", "-d", "-t", "b"]),
            w.run(&["new-session", "-d", "-s", "c"]),
            list(w),
        ];
        w.wait_for_text("b:2", "configured-shell");
        w.wait_for_text("b:3", "configured-shell");
        runs.push(w.run(&["capture-pane", "-p", "-t", "b:2"]));
        runs.push(w.run(&["capture-pane", "-p", "-t", "b:3"]));
        runs.push(w.run(&["show", "-gv", "default-command"]));
        runs.push(w.run(&["show", "-v", "-t", "a", "default-command"]));
        runs
    });
}

/// Owner acceptance: strip only top-level run/run-shell/if-shell, the
/// mu-crew source-file and vtmux-only options, preserving bindings and
/// environment lines. Their
/// effects are later work; source still runs subsequent set lines.
#[test]
fn options_owner_config() {
    compare("options_owner_config", |w| {
        let home = std::env::var_os("HOME").expect("HOME");
        let original =
            std::fs::read_to_string(PathBuf::from(home).join("dotfiles/tmux/.tmux.conf"))
                .unwrap_or_else(|_| {
                    include_str!("../../vtmux-core/src/options/golden/owner_tmux.conf").to_owned()
                });
        let text = original
            .lines()
            .filter(|line| {
                let command = line.split_whitespace().next().unwrap_or("");
                // The `%if "#{vtmux_version}"` block (D23) sets an option
                // tmux has not got: vtmux-only, so it is not compared.
                !(matches!(command, "run" | "run-shell" | "if-shell")
                    || line.contains("vtmux-focus-ring")
                    || command == "source-file" && line.contains("mu-crew.conf"))
            })
            .collect::<Vec<_>>()
            .join("\n");
        let config = w.dir().join("owner.conf");
        std::fs::write(&config, text).expect("write owner copy");
        let started = w.run(&["new", "-d", "-s", "a", "cat"]);
        // Binding/environment handlers are not wired yet; source continues
        // after their per-line errors. This acceptance check is for options.
        let _source = w.run(&["source", config.to_str().expect("utf-8")]);
        vec![started, w.run(&["show", "-g"]), w.run(&["show", "-gw"])]
    });
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

/// split-window, kill-pane, select-pane, last-pane and list-panes on an
/// 80x24 window, against the reference: geometry and pane order after
/// -h/-v/-b/-f/-l/-p splits, `-P`/`-F`, the active and last-used panes,
/// directional selection, `-d`/`-e` input and `-T` titles, the errors
/// (`no space for a new pane`, layout.c:1692; `invalid tiled geometry`,
/// layout.c:1681; `no last pane`, cmd-select-pane.c:176; `can't find pane`;
/// `-f only valid with -a`; `invalid sort order`) and the list-panes orders
/// (sort.c:207-250). list-panes is compared with -F only: the default
/// template has `#{history_bytes}`, tmux's grid memory (D-001).
#[test]
fn panes_split_select_kill() {
    compare("panes_split_select_kill", |w| {
        let lsp = "#{pane_index} #{pane_id} #{pane_left},#{pane_top} #{pane_right},#{pane_bottom} \
                   #{pane_width}x#{pane_height} #{pane_at_top}#{pane_at_bottom}#{pane_at_left}#{pane_at_right} \
                   #{pane_active}#{pane_last}";
        let mut runs = vec![
            w.run(&[
                "new", "-d", "-s", "a", "-n", "w", "-x", "80", "-y", "24", "cat",
            ]),
            w.run(&["lastp"]),
            w.run(&["splitw", "-t", "a", "cat"]),
            w.run(&[
                "display",
                "-p",
                "#{window_panes} #{pane_index} #{window_layout}",
            ]),
            w.run(&["lastp"]),
            w.run(&["display", "-p", "#{pane_id}"]),
            w.run(&["splitw", "-h", "-d", "-P", "cat"]),
            w.run(&[
                "splitw",
                "-v",
                "-b",
                "-d",
                "-l",
                "3",
                "-t",
                "%0",
                "-P",
                "-F",
                "#{pane_id}",
                "cat",
            ]),
            w.run(&["splitw", "-f", "-h", "-d", "-p", "25", "cat"]),
            w.run(&["splitw", "-bf", "-d", "-l", "20%", "cat"]),
            w.run(&["lsp", "-F", lsp]),
            w.run(&["display", "-p", "#{window_layout}"]),
            w.run(&["splitw", "-l", "abc"]),
            w.run(&["splitw", "-p", "200"]),
            w.run(&["splitw", "-l", ""]),
            w.run(&["splitw", "-t", "%99"]),
            w.run(&["selectp", "-L"]),
            w.run(&["display", "-p", "L #{pane_id}"]),
            w.run(&["selectp", "-U"]),
            w.run(&["display", "-p", "U #{pane_id}"]),
            w.run(&["selectp", "-R"]),
            w.run(&["display", "-p", "R #{pane_id}"]),
            w.run(&["selectp", "-D"]),
            w.run(&["display", "-p", "D #{pane_id}"]),
            w.run(&["selectp", "-t", "%1", "-T", "t#(x)#{pane_id}"]),
            w.run(&["display", "-p", "-t", "%1", "[#{pane_title}]"]),
            w.run(&["selectp", "-l"]),
            w.run(&["display", "-p", "#{pane_id} #{pane_last}"]),
            w.run(&["lsp", "-O", "bad"]),
            w.run(&["lsp", "-O", "size", "-F", "#{pane_id}"]),
            w.run(&["lsp", "-O", "size", "-r", "-F", "#{pane_id}"]),
            w.run(&["lsp", "-O", "index", "-r", "-F", "#{pane_id}"]),
            w.run(&["lsp", "-O", "activity", "-F", "#{pane_id}"]),
            w.run(&["lsp", "-O", "creation", "-r", "-F", "#{pane_id}"]),
            w.run(&["lsp", "-O", "title", "-F", "#{pane_id}"]),
            w.run(&["lsp", "-O", "order", "-r", "-F", "#{pane_id}"]),
            w.run(&["lsp", "-r", "-F", "#{pane_id}"]),
            w.run(&[
                "lsp",
                "-a",
                "-F",
                "#{session_name}:#{window_index}.#{pane_index} #{line}",
            ]),
            w.run(&["lsp", "-s", "-f", "#{pane_active}", "-F", "#{pane_id}"]),
            w.run(&["killp", "-f", "x"]),
            w.run(&["killp", "-t", "%99"]),
            w.run(&["killp", "-t", "%3"]),
            w.run(&["lsp", "-F", lsp]),
            w.run(&["killp", "-a", "-f", "#{==:#{pane_id},%2}"]),
            w.run(&["lsp", "-F", lsp]),
            w.run(&["killp", "-a"]),
            w.run(&["lsp", "-F", lsp]),
            w.run(&["display", "-p", "#{window_layout}"]),
        ];
        // select-pane -d: send-keys reaches nothing until -e
        // (window.c:2095 window_pane_key).
        runs.push(w.run(&["splitw", "-d", "cat"]));
        runs.push(w.run(&["selectp", "-d", "-t", "%5"]));
        runs.push(w.run(&["display", "-p", "-t", "%5", "#{pane_input_off}"]));
        runs.push(w.run(&["send", "-t", "%5", "-l", "lost"]));
        runs.push(w.run(&["selectp", "-e", "-t", "%5"]));
        runs.push(w.run(&["send", "-t", "%5", "-l", "kept"]));
        w.wait_for_text("%5", "kept");
        runs.push(w.run(&["capturep", "-p", "-t", "%5", "-E", "0"]));
        runs.push(w.run(&["killp"]));
        runs.push(w.run(&["killp"]));
        w.wait_gone();
        runs.push(w.run(&["ls"]));
        runs
    });
}

/// Pane target forms with pane-base-index 1 and then 0 (cmd-find.c:579-660,
/// window.c `window_find_string`, `window_pane_find_*`), and what a failed
/// CANFAIL lookup leaves for display-message (cmd-find.c:1237-1243,
/// format.c `format_defaults`).
#[test]
fn pane_targets() {
    compare("pane_targets", |w| {
        let mut runs = vec![
            w.run(&[
                "new", "-d", "-s", "a", "-n", "w", "-x", "80", "-y", "24", "cat",
            ]),
            w.run(&["setw", "-g", "pane-base-index", "1"]),
            w.run(&["splitw", "-t", "a", "cat"]),
            w.run(&["display", "-p", "#{pane_index} #{window_layout}"]),
        ];
        let targets = [
            "a.1",
            "a.0",
            "a.2",
            "a.{bottom}",
            "a.{down-of}",
            "a.{up-of}",
            "a.{top}",
            "a.!",
            "a.+",
            "a.-1",
            "{last}",
            "{next}",
            "{previous}",
        ];
        for t in targets {
            runs.push(w.run(&["display", "-p", "-t", t, "#{pane_id}"]));
        }
        runs.push(w.run(&["setw", "-g", "pane-base-index", "0"]));
        runs.push(w.run(&["splitw", "-h", "-d", "cat"]));
        runs.push(w.run(&["splitw", "-v", "-t", "%0", "-d", "-l", "3", "cat"]));
        let targets = [
            "{up-of}",
            "{down-of}",
            "{left-of}",
            "{right-of}",
            "{top}",
            "{bottom}",
            "{left}",
            "{right}",
            "{top-left}",
            "{top-right}",
            "{bottom-left}",
            "{bottom-right}",
            "!",
            "+",
            "-",
            "+2",
            "-3",
            "0",
            "3",
            "4",
            "a.7",
            "nope",
            "a:9",
            ":9",
            ".7",
            "%9",
            "a:w.7",
            "a:x.1",
            "7",
            "w",
            "@9",
            "@0.5",
            "$9",
            "$0:7",
        ];
        for t in targets {
            runs.push(w.run(&["display", "-p", "-t", t, "#{pane_id}"]));
        }
        for t in ["a.{left-of}", "a.9", "{bottom-left}", "!"] {
            runs.push(w.run(&["selectp", "-t", t]));
            runs.push(w.run(&["display", "-p", "#{pane_id}"]));
        }
        // With a pane status line the edges and positions move
        // (window_get_pane_status).
        runs.push(w.run(&["setw", "pane-border-status", "top"]));
        runs.push(w.run(&[
            "lsp",
            "-F",
            "#{pane_id} #{pane_top} #{pane_height} #{pane_at_top}#{pane_at_bottom}",
        ]));
        runs.push(w.run(&["display", "-p", "-t", "a.{top}", "#{pane_id}"]));
        runs.push(w.run(&["setw", "pane-border-status", "bottom"]));
        runs.push(w.run(&[
            "lsp",
            "-F",
            "#{pane_id} #{pane_top} #{pane_height} #{pane_at_top}#{pane_at_bottom}",
        ]));
        runs.push(w.run(&["display", "-p", "-t", "a.{bottom}", "#{pane_id}"]));
        runs.push(w.run(&["display", "-p", "-t", "a.{down-of}", "#{pane_id}"]));
        runs
    });
}

/// resize-pane -x/-y (cells and percentages), -U/-D/-L/-R with and
/// without counts (a positional count, cmd-resize-pane.c:152-158), the
/// errors (`width invalid`, `height too small`, `adjustment too large`,
/// arguments.c `args_string_percentage`), and zoom: -Z toggles,
/// `window_zoomed_flag` and `window_flags` `*Z`, the saved layout in
/// `#{window_layout}` against `#{window_visible_layout}`, and what unzooms
/// (resize-pane, select-pane to a hidden pane, split-window, kill-pane) or
/// re-zooms (select-pane -Z, last-pane -Z, split-window -Z) (window.c
/// `window_zoom`, `window_push_zoom`, `window_pop_zoom`).
#[test]
fn panes_resize_and_zoom() {
    compare("panes_resize_and_zoom", |w| {
        let l =
            "#{pane_id} #{pane_left},#{pane_top} #{pane_width}x#{pane_height} #{pane_zoomed_flag}";
        let z = "#{window_zoomed_flag} #{window_flags} #{pane_id} #{window_panes}";
        let steps: &[&[&str]] = &[
            &[
                "new", "-d", "-s", "a", "-n", "w", "-x", "80", "-y", "24", "cat",
            ],
            &["splitw", "-d", "cat"],
            &["splitw", "-h", "-d", "-t", "%0", "cat"],
            &["resizep", "-t", "%0", "-R", "5"],
            &["resizep", "-t", "%0", "-D", "2"],
            &["resizep", "-t", "%2", "-L", "3", "-U", "1"],
            &["lsp", "-F", l],
            &["resizep", "-t", "%1", "-U"],
            &["resizep", "-t", "%1", "-D", "4"],
            &["resizep", "-t", "%2", "-L4"],
            &["lsp", "-F", l],
            &["resizep", "-t", "%0", "-x", "50%"],
            &["resizep", "-t", "%1", "-y", "30%"],
            &["lsp", "-F", l],
            &["resizep", "-t", "%0", "-x", "10", "-y", "5"],
            &["resizep", "-t", "%0", "-U", "5", "3"],
            &["lsp", "-F", l],
            &["resizep", "-t", "%0", "-x", "0"],
            &["resizep", "-t", "%0", "-y", "1000"],
            &["lsp", "-F", l],
            &["resizep", "-x", "abc"],
            &["resizep", "-y", "-1"],
            &["resizep", "-x", "1001%"],
            &["resizep", "-x", ""],
            &["resizep", "-U", "abc"],
            &["resizep", "-U", "99999999999"],
            &["resizep", "-x", "20", "-L", "x"],
            &["lsp", "-F", l],
            &["display", "-p", "#{window_layout}"],
            &["resizep", "-Z", "-t", "%2"],
            &["display", "-p", z],
            &["lsp", "-F", l],
            &["display", "-p", "#{window_layout}|#{window_visible_layout}"],
            &["resizep", "-t", "%0", "-R", "2"],
            &["display", "-p", z],
            &["resizep", "-Z", "-t", "%1"],
            &["resizep", "-Z"],
            &["display", "-p", z],
            &["resizep", "-Z", "-t", "%1"],
            &["selectp", "-t", "%0"],
            &["display", "-p", z],
            &["resizep", "-Z"],
            &["selectp", "-t", "%1", "-Z"],
            &["display", "-p", z],
            &["lastp", "-Z"],
            &["display", "-p", z],
            &["lastp"],
            &["display", "-p", z],
            &["resizep", "-Z"],
            &["selectp", "-L"],
            &["display", "-p", z],
            &["resizep", "-Z", "-t", "%0"],
            &["splitw", "-d", "cat"],
            &["display", "-p", z],
            &["lsp", "-F", l],
            &["resizep", "-Z"],
            &["splitw", "-Z", "cat"],
            &["display", "-p", z],
            &["splitw", "-Z", "-d", "-h", "cat"],
            &["display", "-p", z],
            &["lsp", "-F", l],
            &["killp"],
            &["display", "-p", z],
            &["lsp", "-F", l],
            &["resizep", "-Z"],
            &["killp", "-a"],
            &["display", "-p", z],
            &["resizep", "-Z"],
            &["display", "-p", z],
        ];
        steps.iter().map(|a| w.run(a)).collect()
    });
}

/// The pane program sees its cell size: `stty size` in the pane agrees
/// with `#{pane_height} #{pane_width}` after a split, a resize, a zoom and
/// an unzoom (`window_pane_resize` -> `TIOCSWINSZ`), including a pane that is
/// on the alternate screen (alternate-screen-resize.sh).
#[test]
fn panes_pty_sizes() {
    compare("panes_pty_sizes", |w| {
        // `alt` puts the program on the alternate screen first.
        let lp = "while read x; do [ \"$x\" = alt ] && printf '\\033[?1049h'; stty size; done";
        let mut runs = vec![w.run(&[
            "new", "-d", "-s", "a", "-n", "w", "-x", "50", "-y", "20", lp,
        ])];
        runs.push(Run::seen(w.pty_size_agrees("%0")));
        runs.push(w.run(&["splitw", "-h", "-d", lp]));
        runs.push(Run::seen(w.pty_size_agrees("%0")));
        runs.push(Run::seen(w.pty_size_agrees("%1")));
        runs.push(w.run(&["splitw", "-v", "-d", "-t", "%0", "-l", "30%", lp]));
        runs.push(Run::seen(w.pty_size_agrees("%0")));
        runs.push(Run::seen(w.pty_size_agrees("%2")));
        runs.push(w.run(&["resizep", "-t", "%1", "-L", "7"]));
        runs.push(Run::seen(w.pty_size_agrees("%0")));
        runs.push(Run::seen(w.pty_size_agrees("%1")));
        runs.push(w.run(&["resizep", "-Z", "-t", "%2"]));
        runs.push(Run::seen(w.pty_size_agrees("%2")));
        runs.push(w.run(&["resizep", "-Z"]));
        runs.push(Run::seen(w.pty_size_agrees("%2")));
        runs.push(w.run(&["killp", "-t", "%0"]));
        runs.push(Run::seen(w.pty_size_agrees("%2")));
        // A program on the alternate screen still gets the new size.
        runs.push(w.run(&["send-keys", "-t", "%1", "alt", "Enter"]));
        w.wait_alternate("%1");
        runs.push(w.run(&["resizep", "-t", "%1", "-x", "20"]));
        runs.push(Run::seen(w.pty_size_agrees("%1")));
        runs
    });
}

/// wait-for (cmd-wait-for.c): a waiter blocks until `-S`, which wakes
/// every waiter and runs the rest of its command list; a signal with no
/// waiters is kept for the next wait, and a second one drops it; `-L`
/// queues behind the holder and `-U` hands the lock over; `-w` wakes one
/// waiter by client name, unknown names are fine; kill-server releases
/// everyone. Errors: `channel X not locked`, too many arguments.
#[test]
fn wait_for_channels_and_locks() {
    compare("wait_for_channels_and_locks", |w| {
        let finish = |c: std::process::Child| {
            let out = c.wait_with_output().expect("wait");
            Run {
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                code: out.status.code().unwrap_or(-1),
            }
        };
        let mut runs = vec![
            w.run(&["new", "-d", "-s", "a", "cat"]),
            w.run(&["wait-for", "-U", "x"]),
            w.run(&["wait-for", "-S", "a", "b"]),
            w.run(&["wait-for", "-S", "y"]),
            w.run(&["wait-for", "y"]),
        ];
        // The second waiter starts once the first is listed, so the order
        // of the list is fixed.
        let wa = w.spawn(&["wait-for", "c", ";", "display", "-p", "A"]);
        runs.push(w.waiters(&["c"], &[wa.id()], "client-0\n"));
        let wb = w.spawn(&["wait-for", "c", ";", "display", "-p", "B"]);
        let pids = [wa.id(), wb.id()];
        runs.push(w.waiters(&["c"], &pids, "client-0\nclient-1\n"));
        runs.push(w.run(&["wait-for", "-S", "c"]));
        runs.push(finish(wa));
        runs.push(finish(wb));
        runs.push(w.waiters(&["c"], &pids, ""));
        // Lock handoff.
        runs.push(w.run(&["wait-for", "-L", "l"]));
        let locker = w.spawn(&["wait-for", "-L", "l", ";", "display", "-p", "got"]);
        runs.push(w.waiters(&["l"], &[locker.id()], "client-0\n"));
        runs.push(w.run(&["wait-for", "-U", "l"]));
        runs.push(finish(locker));
        runs.push(w.run(&["wait-for", "-U", "l"]));
        runs.push(w.run(&["wait-for", "-U", "l"]));
        // -w by client name.
        let wk = w.spawn(&["wait-for", "k", ";", "display", "-p", "K"]);
        let name = format!("client-{}", wk.id());
        runs.push(w.waiters(&["k"], &[wk.id()], "client-0\n"));
        runs.push(w.run(&["wait-for", "-w", "nobody", "k"]));
        runs.push(w.run(&["wait-for", "-w", &name, "k"]));
        runs.push(finish(wk));
        // kill-server releases a waiter, which runs the rest of its list.
        let wz = w.spawn(&["wait-for", "z", ";", "display", "-p", "Z"]);
        runs.push(w.waiters(&["z"], &[wz.id()], "client-0\n"));
        runs.push(w.run(&["kill-server"]));
        runs.push(finish(wz));
        runs
    });
}

/// A command that waits inside `source-file` holds the rest of the file
/// and the commands after `source-file` (cmd-source-file.c:131-138 queues
/// the file's commands after it with `cmdq_insert_after`): `wait-for`
/// parks the client's queue until signalled, then the file's later lines,
/// its causes, and the rest of the argv list run in order.
#[test]
fn source_file_waits_inside() {
    compare("source_file_waits_inside", |w| {
        let conf = w.dir().join("wait.conf");
        std::fs::write(
            &conf,
            "display -p one\nwait-for ch\ndisplay -p two\nselectw -t nosuch\ndisplay -p three\n",
        )
        .expect("write");
        let c = conf.to_str().expect("utf-8");
        let mut runs = vec![w.run(&["new", "-d", "-s", "a", "cat"])];
        let waiter = w.spawn(&["source-file", c, ";", "display", "-p", "after"]);
        runs.push(w.waiters(&["ch"], &[waiter.id()], "client-0\n"));
        runs.push(w.run(&["wait-for", "-S", "ch"]));
        let out = waiter.wait_with_output().expect("wait");
        runs.push(Run {
            stdout: w.normalise(&String::from_utf8_lossy(&out.stdout)),
            stderr: w.normalise(&String::from_utf8_lossy(&out.stderr)),
            code: out.status.code().unwrap_or(-1),
        });
        runs
    });
}

/// `run-shell` from the CLI (cmd-run-shell.c): output lines on stdout,
/// a final line without a newline, `'cmd' returned N` / `terminated by
/// signal N` with the client's exit status (`cmd_run_shell_callback`), stderr
/// only with -E, `#{1}` arguments, -c and its fallback to home, -C parse
/// and runtime errors, -d (and `invalid delay time`), the job environment
/// (TERM from default-terminal, TMUX with the session ID), and the exit
/// status only of the last run-shell.
#[test]
fn run_shell_cli() {
    compare("run_shell_cli", |w| {
        let tmp = w.dir().to_str().expect("utf-8").to_owned();
        let mut runs = vec![w.run(&["new", "-d", "-s", "s", "cat"])];
        for args in [
            &["run", "echo foo; echo bar"][..],
            &["run", "printf abc"],
            &["run", "exit 3"],
            &["run", "kill -9 $$"],
            &["run", "echo err >&2"],
            &["run", "-E", "echo err >&2"],
            &["run", "-d", "abc", "echo x"],
            &["run", "-d", "0x", "echo x"],
            &["run", "-d", "0.1", "echo delayed"],
            &["run", "-d", "inf"],
            &["run"],
            &["run", "echo a", ";", "display", "-p", "b"],
            &["run", "-C", "display -p hi"],
            &["run", "-C", "nosuch"],
            &["run", "-C", "{"],
            &[
                "run",
                "-C",
                "display -p a ; display -p b",
                ";",
                "display",
                "-p",
                "c",
            ],
            &["run", "echo #{session_name}:#{1}:#{2}", "x", "y"],
            &[
                "run",
                "echo $TERM $TERM_PROGRAM $COLORTERM; echo $TMUX | cut -d, -f3",
            ],
            // job.c:141-146: a directory it cannot enter falls back to home.
            &[
                "run",
                "-c",
                "/nonexistent/dir",
                "[ \"$(pwd)\" = \"$HOME\" ] && echo home",
            ],
            &["run", "-c", &tmp, "pwd"],
            &["run", "-t", "nosuch", "echo hi"],
            &["run", "exit 3", ";", "run", "true"],
            &["run", "true", ";", "run", "exit 3"],
        ] {
            runs.push(w.run(args));
        }
        runs
    });
}

/// `run-shell` output reaches a UTF-8 client as the bytes the job wrote,
/// not re-encoded (`server_client_print` -> `file_print`,
/// server-client.c:3243-3256), in the output lines and in the
/// `'cmd' returned N` line; a client that is not UTF-8 gets `_`.
#[test]
fn run_shell_non_utf8_output() {
    compare("run_shell_non_utf8_output", |w| {
        let hex = |r: Run, bytes: &[u8]| {
            let mut seen = Run::seen(format!("{bytes:02x?}"));
            seen.stderr = r.stderr;
            seen.code = r.code;
            seen
        };
        let mut runs = vec![w.run(&["new", "-d", "-s", "s", "cat"])];
        for (locale, cmd) in [
            ("C.UTF-8", r"printf '\377x\303\251y\n'"),
            ("C.UTF-8", r"printf '\351'; exit 1"),
            ("C", r"printf '\377x\303\251y\n'"),
        ] {
            let out = Command::new(&w.bin)
                .args(["-L", &w.label, "-f", "/dev/null", "run", cmd])
                .current_dir(w.dir())
                .env_remove("TMUX")
                .env_remove("TMUX_PANE")
                .env_remove("LC_CTYPE")
                .env_remove("LANG")
                .env("LC_ALL", locale)
                .env("TMUX_TMPDIR", w.dir())
                .env("SHELL", "/bin/sh")
                .output()
                .expect("run binary");
            let run = Run {
                stdout: String::new(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                code: out.status.code().unwrap_or(-1),
            };
            runs.push(hex(run, &out.stdout));
        }
        runs
    });
}

/// `kill-server` while a waiting `run-shell` job runs: the panes die, the
/// waiting client is released at once with status 0, new clients find no
/// server, and the server process exits only once the job is done
/// (server.c `server_send_exit`, `server_loop`'s `job_still_running`).
#[test]
fn kill_server_with_running_job() {
    compare("kill_server_with_running_job", |w| {
        let started = w.dir().join("started");
        let pidfile = w.dir().join("pane-pid");
        let (st, pf) = (
            started.to_str().expect("utf-8"),
            pidfile.to_str().expect("utf-8"),
        );
        let mut runs = vec![w.run(&[
            "new",
            "-d",
            "-s",
            "k",
            &format!("echo $$ >{pf}; exec sleep 77"),
        ])];
        let client = w.spawn(&["run", &format!("touch {st}; sleep 2")]);
        let begin = Instant::now();
        while !(started.exists() && pidfile.exists()) && begin.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        runs.push(w.run(&["kill-server"]));
        // The waiting client returns long before its job's 2 s are up.
        let released = Instant::now();
        let out = client.wait_with_output().expect("wait");
        runs.push(Run {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            code: out.status.code().unwrap_or(-1),
        });
        runs.push(Run::seen(format!(
            "released early: {}",
            released.elapsed() < Duration::from_millis(1500)
        )));
        runs.push(w.run(&["new", "-d", "-s", "again"]));
        runs.push(w.run(&["ls", "-F", "#S"]));
        // The pane's process is gone (a zombie counts as gone).
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("pane pid")
            .trim()
            .parse()
            .expect("pid");
        let gone = || {
            std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
                stat.rsplit(") ").next().is_some_and(|r| r.starts_with('Z'))
            })
        };
        let begin = Instant::now();
        while !gone() && begin.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        runs.push(Run::seen(format!("pane gone: {}", gone())));
        w.wait_gone();
        runs
    });
}

/// `run-shell -b` returns at once and its output goes to a pane's view
/// mode (D-009), not the client; `-b -C` commands run later on the global
/// queue. `-t` sends output to the pane too.
#[test]
fn run_shell_background() {
    compare("run_shell_background", |w| {
        let mark = w.dir().join("mark");
        let m = mark.to_str().expect("utf-8");
        let mut runs = vec![
            w.run(&["new", "-d", "-s", "s", "cat"]),
            w.run(&["run", "-b", &format!("sleep 0.2; echo bg >{m}")]),
            w.run(&["run", "-t", "s", "echo to-pane"]),
            w.run(&["run", "-b", "-C", "set -g @bgc done"]),
        ];
        let start = Instant::now();
        while !mark.exists() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        runs.push(Run::seen(
            std::fs::read_to_string(&mark).unwrap_or_default(),
        ));
        runs.push(w.run(&["show", "-gv", "@bgc"]));
        runs
    });
}

/// `if-shell` (cmd-if-shell.c): the shell command's exit status picks the
/// first or second command, string or `{ }` block, formats expanded in the
/// shell command; -F tests the expanded text instead; the chosen commands
/// run right after if-shell and before the rest of the list, their errors
/// dropping only their own list; -b returns at once.
#[test]
fn if_shell_cli() {
    compare("if_shell_cli", |w| {
        let conf = w.dir().join("blocks.conf");
        std::fs::write(
            &conf,
            "if true { set -g @blk yes } { set -g @blk no }\n\
             if -F '#{==:a,b}' { set -g @blkf yes } { set -g @blkf no }\n\
             if false { set -g @one yes }\n",
        )
        .expect("write");
        let c = conf.to_str().expect("utf-8");
        let mut runs = vec![w.run(&["new", "-d", "-s", "s", "cat"])];
        for args in [
            &["if", "true", "display -p yes", "display -p no"][..],
            &["if", "false", "display -p yes", "display -p no"],
            &["if", "false", "display -p yes"],
            &["if", "exit 2", "display -p y", "display -p n"],
            &["if", "-F", "1", "display -p yes", "display -p no"],
            &["if", "-F", "#{==:a,b}", "display -p yes", "display -p no"],
            &["if", "-F", "", "display -p yes"],
            &["if", "-F", "0", "display -p yes"],
            &["if", "true", "{"],
            &["if", "-F", "1", "nosuch"],
            &["if", "true", "nosuch"],
            &[
                "if",
                "true",
                "display -p a ; nosuch ; display -p b",
                ";",
                "display",
                "-p",
                "c",
            ],
            &[
                "if",
                "-b",
                "true",
                "set -g @bg 1",
                ";",
                "display",
                "-p",
                "after",
            ],
            &[
                "if",
                "[ \"#{session_name}\" = s ]",
                "display -p y",
                "display -p n",
            ],
            &[
                "if",
                "true",
                "run \"exit 4\"",
                ";",
                "display",
                "-p",
                "after",
            ],
            &["if", "-t", "nosuch", "true", "display -p y"],
            &["if"],
            &["source", c],
            &["show", "-g", "@blk"],
            &["show", "-g", "@blkf"],
            &["show", "-g", "@one"],
        ] {
            runs.push(w.run(args));
        }
        runs
    });
}

/// The startup config runs through the queue (cfg.c `start_cfg`): if-shell
/// and run-shell wait inside it, a job during the config gets the outside
/// `TERM` (job.c:88-91), and the client's own command runs only after it.
/// Runtime errors become `file:line:` causes, printed by the next
/// source-file; `-b` jobs do not hold the config up.
#[test]
fn startup_config_shell() {
    compare("startup_config_shell", |w| {
        let conf = w.dir().join("shell.conf");
        let mark = w.dir().join("bgmark");
        std::fs::write(
            &conf,
            format!(
                "new -d -s keep cat\n\
                 if '[ \"$TERM\" = xterm-test ]' 'set -g @term outside' 'set -g @term other'\n\
                 run 'sleep 0.2'\n\
                 set -g @after-run yes\n\
                 run -b 'touch {}'\n\
                 run -C nosuch\n\
                 if true nosuch2\n\
                 run 'exit 3'\n\
                 if -F 1 'set -g @f 1'\n",
                mark.display()
            ),
        )
        .expect("write");
        let c = conf.to_str().expect("utf-8");
        let mut runs = vec![
            // start-server is a client command like any other: it waits
            // for the config (cfg_client_done) before it exits.
            w.raw_env(
                &[
                    "-L",
                    &w.label,
                    "-f",
                    c,
                    "start",
                    ";",
                    "show",
                    "-gv",
                    "@after-run",
                ],
                &[("TERM", "xterm-test")],
            ),
            w.run(&["show", "-gv", "@term"]),
            w.run(&["show", "-gv", "@f"]),
            w.run(&["source", "/dev/null"]),
        ];
        let start = Instant::now();
        while !mark.exists() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        runs.push(Run::seen(mark.exists().to_string()));
        runs
    });
}

impl Run {
    /// A value a helper observed, as a run for the transcript.
    fn seen(stdout: String) -> Self {
        Self {
            stdout,
            stderr: String::new(),
            code: 0,
        }
    }
}

/// new-window, select-window, next/previous/last-window, kill-window,
/// rename-window and list-windows on one session (cmd-new-window.c,
/// cmd-select-window.c, cmd-kill-window.c, cmd-rename-window.c,
/// cmd-list-windows.c): the last-window stack and `window_flags` `*`, `-`
/// and `Z` (window.c `window_printable_flags`), the winlink format flags,
/// `session_detach` choosing the next current window (last, else previous,
/// else next), the errors (`no last window`, `no next window`, `no previous
/// window`, `create window failed: index N in use`, `can't specify pane
/// here`, `-f only valid with -a`, `invalid sort order`, `invalid window
/// name`), the default list-windows templates (the `-a` one ends with a
/// space) and the list orders (sort.c `sort_winlink_cmp`). Windows are
/// named with -n: an unnamed `cat` window may still show the name of the
/// forked tmux child until the pane prints (spawn.c, names.c).
#[test]
fn windows_select_kill_list() {
    compare("windows_select_kill_list", |w| {
        let f = "#{window_index} #{window_id} #{window_name} [#{window_flags}] [#{window_raw_flags}] \
                 #{window_active}#{window_last_flag}#{window_start_flag}#{window_end_flag} \
                 #{session_windows} #{window_zoomed_flag}";
        let steps: &[&[&str]] = &[
            &[
                "new", "-d", "-s", "a", "-n", "w0", "-x", "80", "-y", "24", "cat",
            ],
            &["last"],
            &["last", "-a"],
            &["neww", "-d", "-n", "w1", "cat"],
            &["neww", "-n", "w2", "-P", "cat"],
            &[
                "neww",
                "-d",
                "-n",
                "w3",
                "-P",
                "-F",
                "#{window_id} #{window_index}",
                "cat",
            ],
            &["lsw"],
            &["lsw", "-a"],
            &["lsw", "-F", f],
            &["last"],
            &["lsw", "-F", f],
            &["next"],
            &["next"],
            &["display", "-p", "#{window_index}"],
            &["prev"],
            &["prev", "-t", "a"],
            &["display", "-p", "#{window_index}"],
            &["next", "-a"],
            &["prev", "-a"],
            &["selectw", "-t", ":w2"],
            &["selectw", "-T", "-t", ":w2"],
            &["display", "-p", "#{window_index}"],
            &["selectw", "-T", "-t", ":w2"],
            &["display", "-p", "#{window_index}"],
            &["selectw", "-n"],
            &["selectw", "-p"],
            &["selectw", "-l"],
            &["lsw", "-F", f],
            &["selectw", "-t", "99"],
            &["display", "-p", "-t", "{last}", "#{window_name}"],
            &["display", "-p", "-t", ":!", "#{window_name}"],
            &["splitw", "-d", "-t", ":w1", "cat"],
            &["resizep", "-Z", "-t", ":w1"],
            &["lsw", "-F", f],
            &["neww", "-t", "1", "cat"],
            &["neww", "-d", "-t", "a:1.0", "cat"],
            &["neww", "-d", "-t", "a:-9", "cat"],
            &["neww", "-d", "-t", "a:9", "-n", "x", "cat"],
            &["neww", "-d", "-k", "-t", "a:9", "-n", "y", "cat"],
            &["neww", "-d", "-t", "a:+1", "-n", "z", "cat"],
            &["neww", "-d", "-t", "a:-1", "-n", "zz", "cat"],
            &["neww", "-n", "a\tb", "cat"],
            &["lsw", "-F", f],
            &["renamew", "-t", ":y", "y#{window_index}"],
            &["renamew", "bad\tname"],
            &["renamew", "-t", "nope", "x"],
            &["show", "-w", "-t", ":y9", "automatic-rename"],
            &["lsw", "-O", "bogus"],
            &["lsw", "-O", "name", "-F", "#{window_name}"],
            &["lsw", "-O", "name", "-r", "-F", "#{window_name}"],
            &["lsw", "-O", "index", "-r", "-F", "#{window_index}"],
            &["lsw", "-O", "creation", "-r", "-F", "#{window_id}"],
            &["lsw", "-O", "size", "-F", "#{window_id}"],
            &["lsw", "-O", "order", "-r", "-F", "#{window_id} #{line}"],
            &["lsw", "-r", "-F", "#{window_id}"],
            &["lsw", "-f", "#{window_active}", "-F", "#{window_id}"],
            &["killw", "-f", "x"],
            &["killw", "-t", "nope"],
            &["killw"],
            &["lsw", "-F", f],
            &["killw", "-t", ":!"],
            &["lsw", "-F", f],
            &["killw", "-a", "-f", "#{==:#{window_name},w1}"],
            &["lsw", "-F", f],
            &["killw", "-a"],
            &["lsw", "-F", f],
            &["killw", "-a"],
            &["killw"],
        ];
        let mut runs: Vec<Run> = steps.iter().map(|a| w.run(a)).collect();
        w.wait_gone();
        runs.push(w.run(&["ls"]));
        runs
    });
}

/// next/previous-window on a single window fail (session.c `session_next`
/// wraps to the current winlink, `session_set_current` returns 1,
/// cmd-select-window.c:107-116); new-window -a/-b with a free target index
/// creates the window there with no shuffle, because `cmd_find_target` leaves
/// no winlink for it (cmd-find.c:450-459, cmd-new-window.c:130-131), while a
/// `+N` offset keeps the current window and shuffles around it.
#[test]
fn windows_single_and_free_index() {
    compare("windows_single_and_free_index", |w| {
        let f = "#{window_index}:#{window_name}";
        let steps: &[&[&str]] = &[
            &[
                "new", "-d", "-s", "a", "-n", "w0", "-x", "80", "-y", "24", "cat",
            ],
            &["next"],
            &["prev"],
            &["selectw", "-n"],
            &["selectw", "-p"],
            &["neww", "-d", "-n", "w1", "cat"],
            &["neww", "-d", "-a", "-t", "5", "-n", "a5", "cat"],
            &["lsw", "-F", f],
            &["neww", "-d", "-b", "-t", "9", "-n", "b9", "cat"],
            &["lsw", "-F", f],
            &["neww", "-d", "-b", "-t", ":+1", "-n", "bplus", "cat"],
            &["lsw", "-F", f],
            &["neww", "-d", "-a", "-t", ":+3", "-n", "aplus", "cat"],
            &["lsw", "-F", f],
            &["kill-server"],
        ];
        steps.iter().map(|a| w.run(a)).collect()
    });
}

/// base-index and renumber-windows (spawn.c `spawn_window` with idx -1,
/// window.c `winlink_add`/`winlink_shuffle_up`, session.c
/// `session_renumber_windows`, server-fn.c `server_kill_window`): the first
/// window of a new session at base-index 1, -a/-b insertion shuffling the
/// windows above, kills in the middle with renumbering off and on, the
/// current window and last-window stack following a renumber, kill-window
/// -a renumbering every session, and -S selecting an existing window.
/// Pane output updates the window's activity (input.c:1053
/// `window_update_activity`), so `#{W/t:}` and `lsw -O activity` order by it.
#[test]
fn window_activity_follows_pane_output() {
    compare("window_activity_follows_pane_output", |w| {
        let order = |w: &World| {
            vec![
                w.run(&["display", "-p", "#{W/t:#{window_name} }"]),
                w.run(&["lsw", "-O", "activity", "-F", "#{window_name}"]),
            ]
        };
        let mut runs = vec![w.run(&[
            "new", "-d", "-s", "s1", "-n", "a", "cat", ";", "neww", "-d", "-t", "s1:1", "-n", "b",
            "cat", ";", "neww", "-d", "-t", "s1:2", "-n", "c", "cat",
        ])];
        // Activity stamps have one-second resolution in tmux.
        std::thread::sleep(Duration::from_millis(1100));
        runs.push(w.run(&["send-keys", "-t", "s1:1", "hello", "Enter"]));
        w.wait_for_text("s1:1", "hello");
        std::thread::sleep(Duration::from_millis(1100));
        runs.push(w.run(&["select-window", "-t", "s1:2"]));
        std::thread::sleep(Duration::from_millis(1100));
        runs.push(w.run(&["send-keys", "-t", "s1:0", "x", "Enter"]));
        w.wait_for_text("s1:0", "x");
        runs.extend(order(w));
        runs
    });
}

#[test]
fn windows_base_index_and_renumber() {
    compare("windows_base_index_and_renumber", |w| {
        let f = "#{window_index}:#{window_name}#{window_flags}";
        let lsw = |w: &World| w.run(&["lsw", "-F", f]);
        let mut runs = vec![
            // One invocation: `start ; set` alone leaves an empty server that
            // exit-empty may stop before the next client connects, on tmux
            // and vtmux alike ("server exited unexpectedly" flake).
            w.run(&[
                "start",
                ";",
                "set",
                "-g",
                "base-index",
                "1",
                ";",
                "new",
                "-d",
                "-s",
                "a",
                "-n",
                "w1",
                "-x",
                "80",
                "-y",
                "24",
                "cat",
            ]),
            w.run(&["neww", "-d", "-n", "x", "cat"]),
            w.run(&["neww", "-n", "y", "cat"]),
            w.run(&["neww", "-d", "-a", "-t", "1", "-n", "ins", "cat"]),
            w.run(&["neww", "-d", "-b", "-t", "1", "-n", "bef", "cat"]),
            w.run(&["neww", "-d", "-a", "-n", "aft", "cat"]),
            lsw(w),
            w.run(&["display", "-p", "-t", ":!", "#{window_index}"]),
            w.run(&["killw", "-t", ":ins"]),
            lsw(w),
            w.run(&["set", "renumber-windows", "on"]),
            w.run(&["killw", "-t", ":x"]),
            lsw(w),
            w.run(&["killw"]),
            lsw(w),
            w.run(&["last"]),
            lsw(w),
            w.run(&["neww", "-d", "-t", "9", "-n", "far", "cat"]),
            lsw(w),
            w.run(&["neww", "-S", "-n", "far"]),
            w.run(&["neww", "-S", "-d", "-t", "1"]),
            w.run(&["neww", "-S", "-n", "aft", "-P"]),
            lsw(w),
            w.run(&["set", "-g", "renumber-windows", "on"]),
            w.run(&["new", "-d", "-s", "b", "-n", "b1", "cat"]),
            w.run(&["neww", "-d", "-t", "b:5", "-n", "b5", "cat"]),
            w.run(&[
                "lsw",
                "-a",
                "-F",
                "#{session_name}:#{window_index}:#{window_name}",
            ]),
            w.run(&["killw", "-a", "-t", "a:far"]),
            w.run(&[
                "lsw",
                "-a",
                "-F",
                "#{session_name}:#{window_index}:#{window_name}",
            ]),
            w.run(&["set", "-g", "base-index", "2147483647"]),
            w.run(&["neww", "-d", "-t", "b", "-n", "hi", "cat"]),
            w.run(&["neww", "-d", "-a", "-t", "b:2147483647", "cat"]),
            w.run(&["lsw", "-t", "b", "-F", "#{window_index}"]),
        ];
        runs.push(w.run(&["kill-server"]));
        runs
    });
}

/// new-window's spawn side and automatic rename: the trailing command in
/// its three spellings, -c (absolute, and relative to the client's
/// directory), -e, -P with the default template; the window name from the
/// pane's current command once it has printed (names.c
/// `check_window_name`), the `automatic-rename` window option turned off by
/// -n and rename-window (spawn.c:223, cmd-rename-window.c:61) and back on
/// with setw (options.c `options_push_changes`); -x/-y as the session's
/// default-size, which later windows take (resize.c `default_window_size`).
#[test]
// Linux only on CI: macOS runners miss its naming deadline (task macos_attach_parity).
#[cfg_attr(not(target_os = "linux"), ignore = "macos_attach_parity")]
fn windows_spawn_and_names() {
    compare("windows_spawn_and_names", |w| {
        std::fs::create_dir_all(w.dir().join("sub")).expect("mkdir");
        let sub = w.dir().join("sub");
        let sub = sub.to_str().expect("utf-8");
        let mut runs = vec![w.run(&[
            "new",
            "-d",
            "-s",
            "a",
            "-x",
            "30",
            "-y",
            "5",
            "printf go; exec cat",
        ])];
        w.wait_window_name("a:0", "cat");
        runs.push(w.run(&["show", "-wv", "-t", "a:0", "automatic-rename"]));
        runs.push(w.run(&["show", "-t", "a", "default-size"]));
        runs.push(w.run(&[
            "neww",
            "-d",
            "-c",
            sub,
            "-e",
            "FOO=bar",
            "printf '%s:' \"$FOO\"; pwd; exec cat",
        ]));
        w.wait_for_text("a:1", "bar:");
        runs.push(w.run(&["capture-pane", "-p", "-t", "a:1", "-E", "0"]));
        w.wait_window_name("a:1", "cat");
        runs.push(w.run(&["neww", "-d", "-c", "sub", "pwd; exec cat"]));
        w.wait_for_text("a:2", "/");
        runs.push(w.run(&["capture-pane", "-p", "-t", "a:2", "-E", "0"]));
        runs.push(w.run(&["neww", "-d", "-P", "printf x; exec sleep 101"]));
        runs.push(w.run(&["neww", "-d", "--", "printf x; exec sleep 102"]));
        runs.push(w.run(&["neww", "-d", "sh", "-c", "printf x; exec sleep 103"]));
        for t in ["a:3", "a:4", "a:5"] {
            w.wait_window_name(t, "sleep");
        }
        runs.push(w.run(&[
            "lsw",
            "-F",
            "#{window_index} #{window_name} #{window_width}x#{window_height} #{automatic-rename}",
        ]));
        runs.push(w.run(&["renamew", "-t", "a:0", "fixed"]));
        runs.push(w.run(&["neww", "-d", "-n", "given", "printf x; exec cat"]));
        runs.push(w.run(&["send", "-t", "a:0", "x"]));
        w.wait_for_text("a:0", "x");
        runs.push(w.run(&[
            "lsw",
            "-F",
            "#{window_index} #{window_name} #{automatic-rename}",
        ]));
        runs.push(w.run(&["setw", "-t", "a:0", "automatic-rename", "on"]));
        w.wait_window_name("a:0", "cat");
        runs.push(w.run(&[
            "lsw",
            "-F",
            "#{window_index} #{window_name} #{automatic-rename}",
        ]));
        runs
    });
}

/// A window's first automatic-rename check must see the command, not the
/// shell that is about to exec it: `new -d -s a cat; neww -d cat` named the
/// windows `sh` (tmux's check races the exec the same way, but names them
/// `tmux`, `cat` or `sh` run to run, so D-012 pins the settled outcome
/// instead of comparing). Reference probe, SHELL=/bin/sh, isolated:
/// `new -d -s a cat; neww -d cat; neww -d cat; sleep 2; lsw -F
/// '#{window_name} #{pane_current_command}'` is `tmux cat` for the windows
/// that lost the race and `cat cat` for the ones that won; none is `sh`.
#[test]
fn initial_window_name_is_the_command() {
    let w = World::new(&vtmux_bin());
    w.run(&["new", "-d", "-s", "a", "cat"]);
    for _ in 0..5 {
        w.run(&["neww", "-d", "cat"]);
    }
    w.run(&["neww", "-d"]);
    w.run(&["neww", "-d", "sh", "-c", "exec sleep 101"]);
    // Past two name intervals: every check that is going to run has run.
    std::thread::sleep(Duration::from_millis(1500));
    let names = w.run(&[
        "lsw",
        "-F",
        "#{window_name} #{pane_current_command} #{automatic-rename}",
    ]);
    // Each window ends up named after what is actually running in it. Which
    // process that is depends on the shell: bash execs the last command of
    // `sh -c cat` (so `cat`), dash keeps `sh` in front. Either way the name
    // and #{pane_current_command} must agree, and none may be the
    // half-started shell state the race used to leave behind.
    let lines: Vec<&str> = names.stdout.lines().collect();
    assert_eq!(lines.len(), 8, "{}", names.stdout);
    for line in &lines {
        let f: Vec<&str> = line.split(' ').collect();
        assert_eq!(f.len(), 3, "{line}");
        assert_eq!(
            f[0], f[1],
            "window name follows the running command: {line}"
        );
        assert_eq!(f[2], "1", "{line}");
    }
    assert_eq!(
        lines[7].split(' ').next(),
        Some("sleep"),
        "{}",
        names.stdout
    );
}

/// Runs each argv in order (all through `World::run`).
fn steps(w: &World, steps: &[&[&str]]) -> Vec<Run> {
    steps.iter().map(|a| w.run(a)).collect()
}

/// select-layout, next-layout, previous-layout (cmd-select-layout.c:71-154):
/// named layouts and their prefixes, the next/previous cycle from
/// `w->lastlayout`, `-o` undo (twice is a toggle), no name re-applying the
/// last named layout, `-E`, v2 strings that resize the window and set the
/// active and last panes, a v1 string from a control client round-tripped
/// back through select-layout, and the parse errors as `cause: string`
/// (layout-custom.c, json.c).
#[test]
fn layouts_select_and_strings() {
    compare("layouts_select_and_strings", |w| {
        let layout = &[
            "display",
            "-p",
            "#{window_width}x#{window_height} #{window_layout}",
        ][..];
        let lsp = &[
            "lsp",
            "-F",
            "#{pane_id} #{pane_width}x#{pane_height} #{pane_left},#{pane_top} #{pane_active}#{pane_last}",
        ][..];
        let v2 = r#"{"V":2,"L":{"t":"h","w":100,"h":30,"x":0,"y":0,"c":[{"t":"p","w":50,"h":30,"x":0,"y":0,"i":1},{"t":"v","w":49,"h":30,"x":51,"y":0,"c":[{"t":"p","w":49,"h":15,"x":51,"y":0,"a":true,"i":0},{"t":"p","w":49,"h":14,"x":51,"y":16,"l":0,"i":2}]}]}}"#;
        let v1 = "55bb,80x24,0,0{56x24,0,0,0,23x24,57,0[23x12,57,0,1,23x11,57,13,2]}";
        let v1_bad = "55bb,80x24,0,0{56x24,0,0,0,23x24,57,0[23x12,57,0,1,23x11,57,13,3]}";
        let mut runs = steps(
            w,
            &[
                &[
                    "new", "-d", "-s", "a", "-n", "w", "-x", "80", "-y", "24", "cat",
                ],
                &["selectl", "bogus"],
                &["selectl", r#"{"V":3}"#],
                &["selectl", "tiled"],
                &["splitw", "-d", "cat"],
                &["splitw", "-d", "cat"],
                &["selectl", "even-horizontal"],
                layout,
                &["selectl", "-o"],
                layout,
                &["selectl", "-o"],
                layout,
                &["selectl"],
                &["nextl"],
                layout,
                &["prevl"],
                &["prevl"],
                layout,
                &["selectl", "-n"],
                &["selectl", "-p"],
                &["selectl", "-E"],
                layout,
                &["selectl", "even"],
                &["selectl", "main-h"],
                &["selectl", "main-vertical-m"],
                layout,
                &["selectl", "-t", "%2", "-E"],
                layout,
                &["selectl", v2],
                layout,
                lsp,
                &["selectl", v1],
                layout,
                lsp,
                &["selectl", v1_bad],
                &["selectl", "bad,80x24,0,0"],
                &[
                    "selectl",
                    r#"{"V":2,"L":{"t":"p","w":80,"h":24,"x":0,"y":0}}"#,
                ],
                &["selectl", r#"{"V":2"#],
                &[
                    "selectl",
                    r#"{"V":2,"L":{"t":"q","w":80,"h":24,"x":0,"y":0}}"#,
                ],
                &["selectl", "-t", "%5", "tiled"],
                &["selectl", "-t", "a:9", "tiled"],
                &["nextl", "-t", "a:9"],
                &["selectl", "-o", "x"],
                &["resizep", "-Z"],
                &["selectl", "tiled"],
                &["display", "-p", "#{window_zoomed_flag} #{window_layout}"],
                &["killp", "-a"],
                &["selectl", "-E"],
                &["nextl"],
                layout,
            ],
        );
        // The full layout as a v1 string, as a control client sees it, fed
        // back through select-layout after a change.
        runs.extend(steps(
            w,
            &[
                &["splitw", "-h", "-d", "cat"],
                &["splitw", "-v", "-d", "-t", "%2", "cat"],
                &["selectl", "-t", "a", "main-horizontal"],
            ],
        ));
        let current = w.run(&["display", "-p", "#{window_layout}"]).stdout;
        runs.push(w.run(&["selectl", "even-vertical"]));
        runs.push(w.run(&["selectl", current.trim_end()]));
        runs.push(w.run(layout));
        runs.push(w.run(&["kill-server"]));
        runs
    });
}

/// swap-pane (cmd-swap-pane.c:57-210): -D/-U with the tiled neighbour
/// (wrapping), a pane with itself, -s/-t in one window, -d keeping the
/// active pane, across windows of two sessions (cells, sizes, active and
/// last panes), and -Z keeping a zoomed window zoomed.
#[test]
fn panes_swap() {
    compare("panes_swap", |w| {
        let lsp = &[
            "lsp",
            "-a",
            "-F",
            "#{session_name}:#{window_index}.#{pane_index} #{pane_id} #{pane_active}#{pane_last} #{pane_width}x#{pane_height} #{pane_left},#{pane_top}",
        ][..];
        steps(
            w,
            &[
                &[
                    "new", "-d", "-s", "a", "-n", "w", "-x", "80", "-y", "24", "cat",
                ],
                &[
                    "new", "-d", "-s", "b", "-n", "bw", "-x", "80", "-y", "24", "cat",
                ],
                &["swapp", "-t", "a"],
                &["swapp", "-t", "a", "-D"],
                &["splitw", "-d", "-t", "a", "cat"],
                &["splitw", "-d", "-t", "a", "-h", "cat"],
                &["swapp", "-s", "%0", "-t", "%0"],
                &["swapp", "-t", "a", "-D"],
                lsp,
                &["swapp", "-t", "a", "-U"],
                lsp,
                &["swapp", "-s", "%0", "-t", "%3"],
                lsp,
                &["swapp", "-d", "-s", "%2", "-t", "%3"],
                lsp,
                &["splitw", "-t", "b", "cat"],
                &["splitw", "-d", "-t", "b", "-h", "cat"],
                &["swapp", "-s", "a:0.1", "-t", "b:0.0"],
                lsp,
                &["display", "-p", "-t", "a", "#{window_layout}"],
                &["display", "-p", "-t", "b", "#{window_layout}"],
                &["swapp", "-d", "-s", "a:0.0", "-t", "b:0.2"],
                lsp,
                &["resizep", "-Z", "-t", "b:0.1"],
                &["swapp", "-Z", "-t", "b:0.1", "-D"],
                &[
                    "display",
                    "-p",
                    "-t",
                    "b",
                    "#{window_zoomed_flag} #{pane_id}",
                ],
                &["swapp", "-t", "b:0.0", "-U"],
                &[
                    "display",
                    "-p",
                    "-t",
                    "b",
                    "#{window_zoomed_flag} #{pane_id}",
                ],
                &["resizep", "-Z", "-t", "b:0.0"],
                &["swapp", "-Z", "-t", "b:0.0", "-s", "a:0.0"],
                &[
                    "display",
                    "-p",
                    "-t",
                    "b",
                    "#{window_zoomed_flag} #{pane_id}",
                ],
                lsp,
                &["swapp", "-s", "%9"],
                &["swapp", "-t", "%9"],
                &["swapp", "-s", "c:0"],
                &["kill-server"],
            ],
        )
    });
}

/// break-pane (cmd-break-pane.c:89-209) and join-pane/move-pane
/// (cmd-join-pane.c:420-539): a pane out to a new window (default name,
/// automatic-rename kept; -n turns it off), -d, -a/-b shuffling, -P/-F, a
/// one-pane window relinked into another session, the `index in use` and
/// `same index` errors, join and move across windows and sessions with
/// -h -b -f -d -l -p, an emptied window or session going away, and the
/// `source and target panes must be different` and geometry errors.
#[test]
#[allow(clippy::too_many_lines)] // one step list
fn panes_break_join() {
    compare("panes_break_join", |w| {
        let lsw = &[
            "lsw",
            "-a",
            "-F",
            "#{session_name}:#{window_index} #{window_id} #{window_name} [#{window_flags}] #{window_panes} #{automatic-rename}",
        ][..];
        let lsp = &[
            "lsp",
            "-a",
            "-F",
            "#{session_name}:#{window_index}.#{pane_index} #{pane_id} #{pane_active}#{pane_last} #{pane_width}x#{pane_height} #{pane_left},#{pane_top}",
        ][..];
        let here = &[
            "display",
            "-p",
            "#{session_name}:#{window_index}.#{pane_index} #{pane_id}",
        ][..];
        steps(
            w,
            &[
                &[
                    "new", "-d", "-s", "a", "-n", "w", "-x", "80", "-y", "24", "cat",
                ],
                &[
                    "new", "-d", "-s", "b", "-n", "bw", "-x", "80", "-y", "24", "cat",
                ],
                &["splitw", "-t", "a", "-d", "cat"],
                &["splitw", "-t", "a", "-d", "cat"],
                &["breakp", "-t", "a:0", "-s", "a:0.1"],
                &["breakp", "-s", "a:0.1"],
                lsw,
                here,
                &[
                    "breakp", "-d", "-s", "a:0.1", "-t", "a:5", "-n", "five", "-P",
                ],
                &[
                    "breakp",
                    "-s",
                    "a:5",
                    "-t",
                    "a:7",
                    "-P",
                    "-F",
                    "#{window_id} #{pane_id} #{window_index}",
                ],
                lsw,
                &["breakp", "-s", "a:7", "-t", "b", "-n", "moved"],
                &["breakp", "-s", "a:0", "-t", "b:1"],
                &["breakp", "-s", "b:1", "-t", "b:1"],
                &["breakp", "-n", "a:b", "-s", "a:1"],
                &["breakp", "-s", "b:1", "-t", "a:", "-n", "back"],
                lsw,
                &["neww", "-d", "-t", "a:3", "-n", "three", "cat"],
                &["splitw", "-t", "a:0", "-d", "cat"],
                &["splitw", "-t", "a:0", "-d", "cat"],
                &["breakp", "-s", "a:0.1", "-t", "a:3", "-a", "-n", "after"],
                &[
                    "breakp", "-s", "a:0.1", "-t", "a:3", "-b", "-d", "-n", "before",
                ],
                lsw,
                &["breakp", "-s", "a:3", "-t", "b:4", "-n", "single", "-P"],
                lsw,
                here,
                &["joinp"],
                &["joinp", "-s", "a:0", "-t", "a:0"],
                &["joinp", "-s", "a:4", "-t", "a:0"],
                lsw,
                lsp,
                here,
                &[
                    "joinp", "-h", "-b", "-d", "-l", "10", "-s", "b:0", "-t", "a:0.0",
                ],
                lsp,
                &["movep", "-f", "-s", "a:0.0", "-t", "a:0.2"],
                lsp,
                &["movep", "-l", "abc", "-s", "a:0.0", "-t", "a:0.1"],
                &["joinp", "-p", "200", "-s", "a:0.0", "-t", "a:0.1"],
                &["movep", "-l", "200", "-s", "a:0.0", "-t", "a:0.1"],
                &["movep", "-b", "-f", "-h", "-s", "a:0.2", "-t", "a:0.0"],
                &[
                    "joinp", "-v", "-p", "25", "-d", "-s", "a:0.0", "-t", "a:0.2",
                ],
                &["display", "-p", "-t", "a:0", "#{window_layout}"],
                lsp,
                &["joinp", "-t", "a:0.1", "-s", "%99"],
                &["movep", "-X", "3", "-t", "a:0.0"],
                &["movep", "-M"],
                &["resizep", "-Z", "-t", "a:0.0"],
                &["joinp", "-d", "-s", "b:4", "-t", "a:0.1"],
                &[
                    "display",
                    "-p",
                    "-t",
                    "a:0",
                    "#{window_zoomed_flag} #{window_panes}",
                ],
                lsw,
                &["ls", "-F", "#{session_name} #{session_windows}"],
                &["kill-server"],
            ],
        )
    });
}

/// break-pane -a/-b of a one-pane window in its own session: the window's
/// winlink moves across the shuffle (cmd-break-pane.c:120-145 keep `wl`
/// through `winlink_shuffle_up`), so nothing is linked twice or lost; also
/// zoomed and with -P (reviewer-15's repro).
#[test]
fn panes_break_one_pane_shuffle() {
    compare("panes_break_one_pane_shuffle", |w| {
        let lsw = &[
            "lsw",
            "-a",
            "-F",
            "#{session_name}:#{window_index} #{window_id} #{window_name} [#{window_flags}] #{window_panes}",
        ][..];
        let lsp = &[
            "lsp",
            "-a",
            "-F",
            "#{session_name}:#{window_index}.#{pane_index} #{pane_id}",
        ][..];
        steps(
            w,
            &[
                &[
                    "new", "-d", "-s", "a", "-n", "w", "-x", "80", "-y", "24", "cat",
                ],
                &["neww", "-d", "-t", "a:1", "-n", "x", "cat"],
                &["neww", "-d", "-t", "a:2", "-n", "y", "cat"],
                &["breakp", "-s", "a:2", "-t", "a:0", "-b", "-d"],
                lsw,
                lsp,
                &["breakp", "-s", "a:0", "-t", "a:1", "-a", "-d"],
                lsw,
                &["breakp", "-s", "a:2", "-t", "a:0", "-a", "-P"],
                lsw,
                &[
                    "display",
                    "-p",
                    "#{session_name}:#{window_index} #{window_id}",
                ],
                &["neww", "-d", "-t", "a:5", "-n", "z", "cat"],
                &["breakp", "-s", "a:1", "-t", "a:2", "-a", "-d"],
                lsw,
                &[
                    "new", "-d", "-s", "b", "-n", "bw", "-x", "80", "-y", "24", "cat",
                ],
                &["neww", "-d", "-t", "b:1", "-n", "bx", "cat"],
                &["resizep", "-Z", "-t", "b:1"],
                &["breakp", "-s", "b:1", "-t", "b:0", "-b", "-P"],
                lsw,
                lsp,
                &["kill-server"],
            ],
        )
    });
}

/// link-window, unlink-window, move-window, swap-window and rename-session
/// (cmd-move-window.c, cmd-kill-window.c:75-82, cmd-swap-window.c,
/// cmd-rename-session.c) against the reference: a window linked into two
/// sessions (`window_linked*`, one `kill-window` taking both links, a rename
/// seen from both), every flag, `unlink-window` without and with -k,
/// `move-window -r` (quiet on a missing session) and the errors.
#[test]
#[allow(clippy::too_many_lines)] // one step list
fn winlinks_link_unlink_move_swap_rename() {
    compare("winlinks_link_unlink_move_swap_rename", |w| {
        let lsw = &[
            "lsw",
            "-a",
            "-F",
            "#{session_name}:#{window_index} #{window_id} #{window_name} [#{window_flags}] linked=#{window_linked} #{window_linked_sessions} #{window_linked_sessions_list} #{session_many_attached}",
        ][..];
        steps(
            w,
            &[
                &[
                    "new", "-d", "-s", "a", "-n", "w0", "-x", "80", "-y", "24", "cat",
                ],
                &["neww", "-d", "-t", "a:1", "-n", "w1", "cat"],
                &["neww", "-d", "-t", "a:2", "-n", "w2", "cat"],
                &["new", "-d", "-s", "b", "-n", "b0", "cat"],
                &["linkw", "-s", "a:1", "-t", "b:5"],
                lsw,
                &["linkw", "-s", "a:1", "-t", "b:5"],
                &["linkw", "-s", "a:2", "-t", "b:5"],
                &["linkw", "-k", "-s", "a:2", "-t", "b:5"],
                &["linkw", "-a", "-d", "-s", "a:0", "-t", "b:0"],
                lsw,
                &["rename-window", "-t", "a:2", "shared"],
                lsw,
                &["unlinkw", "-t", "a:1"],
                &["unlinkw", "-t", "b:5"],
                &["unlinkw", "-t", "b:5"],
                &["movew", "-r", "-t", "nosuch"],
                &["movew", "-s", "a:0", "-t", "b:9"],
                &["movew", "-r", "-t", "b"],
                lsw,
                &["swapw", "-d", "-s", "a:1", "-t", "b:1"],
                &["swapw", "-s", "a:0", "-t", "a:1"],
                lsw,
                &["kill-window", "-t", "b:1"],
                lsw,
                &["rename", "-t", "a", "alpha"],
                &["rename", "-t", "b", "alpha"],
                &["rename", "-t", "alpha", "bad\ttab"],
                &["ls", "-F", "#{session_name} #{session_windows}"],
                &["unlinkw", "-k", "-t", "alpha:0"],
                &["unlinkw", "-k", "-t", "b:0"],
                &["kill-server"],
            ],
        )
    });
}

/// respawn-pane (cmd-respawn-pane.c:46-100, spawn.c:312-345): a live pane
/// without -k is an error; -k keeps the pane ID, place, title and history,
/// takes a new command, -c and -e; no command reuses the last one; the old
/// process's exit leaves the respawned pane alone; the screen is reset
/// (alternate screen off, cursor home).
#[test]
fn panes_respawn() {
    compare("panes_respawn", |w| {
        let mut runs = vec![w.run(&[
            "new",
            "-d",
            "-s",
            "a",
            "-n",
            "w",
            "-x",
            "60",
            "-y",
            "10",
            "seq 1 12; printf '\\033]2;mytitle\\033\\\\'; exec cat",
        ])];
        w.wait_for_text("a:0", "12");
        runs.extend(steps(
            w,
            &[
                &["display", "-p", "#{history_size} [#{pane_title}]"],
                &["respawnp"],
                &["respawnp", "-t", "a:0.0"],
                &["respawnp", "-E", "-t", "a:0.0"],
                &["respawnp", "-k", "-t", "%77"],
                &["respawnp", "-k", "printf X; exec cat"],
            ],
        ));
        w.wait_for_text("a:0", "X");
        runs.extend(steps(
            w,
            &[
                &["display", "-p", "#{pane_id} #{history_size} [#{pane_title}] #{cursor_x},#{cursor_y} #{pane_start_command}"],
                &["capturep", "-p"],
                &["capturep", "-p", "-S", "-", "-E", "-1"],
                &["respawnp", "-k", "printf '\\033[?1049hA'; exec cat"],
            ],
        ));
        w.wait_alternate("a:0");
        runs.push(w.run(&["respawnp", "-k", "printf B; exec cat"]));
        w.wait_for_text("a:0", "B");
        runs.extend(steps(
            w,
            &[
                &["display", "-p", "#{alternate_on} #{pane_id}"],
                &["splitw", "-d", "-t", "a:0", "cat"],
            ],
        ));
        std::fs::create_dir_all(w.dir().join("sub")).expect("mkdir");
        let sub = w.dir().join("sub");
        let sub = sub.to_str().expect("utf-8");
        runs.push(w.run(&[
            "respawnp",
            "-k",
            "-c",
            sub,
            "-e",
            "FOO=bar",
            "-t",
            "a:0.1",
            "printf '%s:' \"$FOO\"; pwd; exec cat",
        ]));
        w.wait_for_text("a:0.1", "bar:");
        runs.push(w.run(&["capturep", "-p", "-t", "a:0.1", "-E", "0"]));
        // No -e this time; -c sticks (the pane's directory).
        runs.push(w.run(&["respawnp", "-k", "-t", "a:0.1"]));
        w.wait_for_text("a:0.1", ":TMP/sub");
        runs.extend(steps(
            w,
            &[
                &["capturep", "-p", "-t", "a:0.1", "-E", "0"],
                &[
                    "lsp",
                    "-F",
                    "#{pane_index} #{pane_id} #{pane_dead} #{pane_start_command}",
                ],
                // A pane respawned with a command that exits goes away.
                &["respawnp", "-k", "-t", "a:0.1", "exit 0"],
            ],
        ));
        let start = Instant::now();
        while w.run(&["display", "-p", "#{window_panes}"]).stdout != "1\n"
            && start.elapsed() < Duration::from_secs(5)
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        runs.extend(steps(
            w,
            &[&["lsp", "-F", "#{pane_index} #{pane_id}"], &["kill-server"]],
        ));
        runs
    });
}

/// rotate-window (cmd-rotate-window.c:44-114): panes shift along their
/// cells (list order, ids, geometry), -D and -U, the active and last
/// panes, a zoomed window unzoomed unless -Z, and the target errors.
#[test]
fn windows_rotate() {
    compare("windows_rotate", |w| {
        let lsp = &[
            "lsp",
            "-F",
            "#{pane_index} #{pane_id} #{pane_active}#{pane_last} #{pane_width}x#{pane_height} #{pane_left},#{pane_top}",
        ][..];
        let zoom = &["display", "-p", "#{window_zoomed_flag} #{pane_id}"][..];
        steps(
            w,
            &[
                &[
                    "new", "-d", "-s", "a", "-n", "w", "-x", "80", "-y", "24", "cat",
                ],
                &["rotatew"],
                &["splitw", "-d", "cat"],
                &["splitw", "-d", "-h", "cat"],
                &["splitw", "-d", "-t", "a:0.0", "-l", "3", "cat"],
                lsp,
                &["rotatew"],
                lsp,
                &["rotatew", "-D"],
                &["rotatew", "-D"],
                lsp,
                &["rotatew", "-D", "-U"],
                lsp,
                &["selectp", "-t", "a:0.2"],
                &["rotatew", "-U"],
                lsp,
                &["selectl", "main-vertical"],
                &["rotatew", "-D"],
                lsp,
                &["resizep", "-Z", "-t", "a:0.1"],
                &["rotatew"],
                zoom,
                &["resizep", "-Z", "-t", "a:0.1"],
                &["rotatew", "-Z"],
                zoom,
                lsp,
                &["rotatew", "-t", "nosuch"],
                &["rotatew", "-t", "a:9"],
                &["rotatew", "extra"],
                &["kill-server"],
            ],
        )
    });
}

/// respawn-window (cmd-respawn-window.c:46-110, spawn.c:106-128): a live
/// window without -k is an error (vtmux has no remain-on-exit); -k keeps
/// the window ID and index, leaves the first pane alone in a full-size
/// layout, takes a command, -c and -e, and selects the window.
#[test]
fn windows_respawn() {
    compare("windows_respawn", |w| {
        let mut runs = steps(
            w,
            &[
                &[
                    "new", "-d", "-s", "a", "-n", "w", "-x", "80", "-y", "24", "cat",
                ],
                &["new", "-d", "-s", "keep", "cat"],
                &["neww", "-d", "-t", "a", "-n", "two", "cat"],
                &["splitw", "-d", "-t", "a:1", "cat"],
                &["splitw", "-d", "-t", "a:1", "-h", "cat"],
                &["selectp", "-t", "a:1.1"],
                &["respawnw", "-t", "a:1"],
                &["respawnw", "-k", "-t", "a:9"],
                &["respawnw", "-k", "-t", "a:1", "printf X; exec cat"],
            ],
        );
        w.wait_for_text("a:1", "X");
        runs.extend(steps(
            w,
            &[
                &[
                    "lsw",
                    "-t",
                    "a",
                    "-F",
                    "#{window_index} #{window_id} #{window_active}#{window_last_flag} #{window_panes} #{window_layout}",
                ],
                &[
                    "lsp",
                    "-t",
                    "a:1",
                    "-F",
                    "#{pane_index} #{pane_id} #{pane_width}x#{pane_height} #{pane_start_command}",
                ],
                &["splitw", "-d", "-t", "a:1", "cat"],
                &["lsp", "-t", "a:1", "-F", "#{pane_id}"],
            ],
        ));
        runs.push(w.run(&[
            "respawnw",
            "-k",
            "-c",
            "/tmp",
            "-e",
            "FOO=bar",
            "-t",
            "a:1",
            "printf '%s:' \"$FOO\"; pwd; exec cat",
        ]));
        w.wait_for_text("a:1", "bar:/tmp");
        runs.extend(steps(
            w,
            &[&["lsp", "-t", "a:1", "-F", "#{pane_id}"], &["kill-server"]],
        ));
        runs
    });
}

/// respawn-window -k on a zoomed window (D-008): tmux leaves the zoom flag
/// on freed cells and dies on the next layout read; vtmux unzooms.
#[test]
fn windows_respawn_zoomed() {
    let (_keep, as_tmux) = vtmux_as_tmux();
    let w = World::new(&as_tmux);
    let out = |args: &[&str]| w.run(args).stdout;
    assert_eq!(
        w.run(&["new", "-d", "-s", "a", "-x", "80", "-y", "24", "cat"])
            .code,
        0
    );
    w.run(&["splitw", "-d", "cat"]);
    w.run(&["resizep", "-Z", "-t", "a:0.1"]);
    assert_eq!(out(&["display", "-p", "#{window_zoomed_flag}"]), "1\n");
    assert_eq!(w.run(&["respawnw", "-k"]).code, 0);
    assert_eq!(
        out(&[
            "display",
            "-p",
            "#{window_zoomed_flag} #{window_panes} #{pane_id}"
        ]),
        "0 1 %0\n"
    );
    assert_eq!(
        out(&["display", "-p", "#{window_layout}"]),
        "{\"V\":2,\"L\":{\"t\":\"p\",\"w\":80,\"h\":24,\"x\":0,\"y\":0,\"a\":true,\"i\":0,\"I\":\"%0\"}}\n"
    );
    w.run(&["kill-server"]);
}

/// set-environment / show-environment (cmd-set-environment.c,
/// cmd-show-environment.c): both scopes, `-h`, `-r`, `-u`, `-F`, `-s` and
/// its escaping, listing, every argument and target error. `cat` keeps the
/// session alive.
#[test]
fn environment_commands() {
    compare("environment_commands", |w| {
        steps(
            w,
            &[
                &["new", "-d", "-s", "main", "cat"],
                &["setenv", "FOO", "bar"],
                &["showenv", "FOO"],
                &["set-environment", "-g", "GVAR", "gval"],
                &["show-environment", "-g", "GVAR"],
                &["show-environment", "-g", "FOO"],
                &["set-environment", "FOO", "baz"],
                &["show-environment", "FOO"],
                &["set-environment", "ESC", "a$b`c\"d\\e"],
                &["show-environment", "-s", "ESC"],
                &["show-environment", "ESC"],
                &[
                    "set-environment",
                    "-t",
                    "main",
                    "-F",
                    "EXP",
                    "#{session_name}",
                ],
                &["show-environment", "EXP"],
                &["set-environment", "-h", "SECRET", "s3cr"],
                &["show-environment", "SECRET"],
                &["show-environment", "-h", "SECRET"],
                &["show-environment", "-h", "FOO"],
                &["set-environment", "-r", "FOO"],
                &["show-environment", "FOO"],
                &["show-environment", "-s", "FOO"],
                &["show-environment", "-hs", "SECRET"],
                &["set-environment", "-u", "FOO"],
                &["show-environment", "FOO"],
                &["set-environment", "-u", "NEVERSET"],
                &["set-environment", "-r", "ONLYCLEARED"],
                &["show-environment", "ONLYCLEARED"],
                &["set-environment", "-g", "LISTA", "1"],
                &["set-environment", "-g", "LISTB", "2"],
                &["set-environment", "-gh", "LISTHID", "3"],
                &["set-environment", "-g", "-r", "LISTC"],
                &["show-environment", "-g", "LISTA"],
                &["show-environment", "-gh"],
                &["show-environment", "-s", "-t", "main"],
                &["show-environment", "-t", "main"],
                &["set-environment", "", "x"],
                &["set-environment", "A=B", "x"],
                &["set-environment", "-u", "FOO", "val"],
                &["set-environment", "-r", "FOO", "val"],
                &["set-environment", "NOVAL"],
                &[
                    "set-environment",
                    "-F",
                    "FE",
                    "#{session_name}-#{pane_index}",
                ],
                &["show-environment", "FE"],
                &["set-environment", "-g", "-F", "GE", "#{session_name}"],
                &["show-environment", "-g", "GE"],
                &["show-environment", "MISSING"],
                &["show-environment", "-t", "nosuch", "FOO"],
                &["set-environment", "-t", "nosuch", "FOO", "bar"],
                &["set-environment", "-g", "-t", "nosuch", "G2", "v"],
                &["show-environment", "-g", "-t", "nosuch", "G2"],
                &["show-environment", "-g", "-t", "nosuch"],
                &["set-environment"],
                &["show-environment", "a", "b"],
                &["show-environment", "-z"],
            ],
        )
        .into_iter()
        .map(|mut r| {
            // The global environment is the client's: only compare the
            // variables this test set.
            if r.stdout.lines().count() > 20 {
                r.stdout = r
                    .stdout
                    .lines()
                    .filter(|l| l.starts_with("LIST") || l.starts_with("GVAR"))
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            r
        })
        .collect()
    });
}

/// A pane started after set-environment sees the variable; `-r` and `-u`
/// ones, and hidden ones, are absent (`environ_for_session`, `environ_push`).
/// The session scope beats the global one, and a session `-r` hides a global
/// variable (`environ_copy` of a removal).
#[test]
fn environment_reaches_new_panes() {
    compare("environment_reaches_new_panes", |w| {
        let show = "printf '[%s|%s|%s|%s|%s|%s|%s]' \"$SV\" \"$GV\" \"$HV\" \"$RV\" \"$UV\" \"$BOTH\" \"${GR-unset}\"; exec cat";
        let mut runs = steps(
            w,
            &[
                &["new", "-d", "-s", "a", "cat"],
                &["setenv", "SV", "session"],
                &["setenv", "-g", "GV", "global"],
                &["setenv", "-h", "HV", "hidden"],
                &["setenv", "RV", "removed"],
                &["setenv", "-r", "RV"],
                &["setenv", "UV", "unset"],
                &["setenv", "-u", "UV"],
                &["setenv", "-g", "BOTH", "global"],
                &["setenv", "BOTH", "session"],
                &["setenv", "-g", "GR", "g"],
                &["setenv", "-r", "GR"],
                &["neww", "-d", "-n", "e", show],
            ],
        );
        w.wait_for_text("a:e", "[");
        runs.push(w.run(&["capturep", "-p", "-t", "a:e"]));
        runs.push(w.run(&["splitw", "-d", "-t", "a:e", show]));
        w.wait_for_text("a:e.1", "[");
        runs.push(w.run(&["capturep", "-p", "-t", "a:e.1"]));
        runs
    });
}

/// `update-environment` with a custom list is applied by new-session (not
/// with `-E`), including fnmatch patterns and the removal marker for a
/// pattern that matches nothing (cmd-new-session.c:282-283, environ.c:186).
#[test]
fn update_environment_on_new_session() {
    compare("update_environment_on_new_session", |w| {
        let env = [
            ("MYVAR", "fromclient"),
            ("TEST_GLOB", "globval"),
            ("OTHER", "nope"),
        ];
        let mut runs = steps(
            w,
            &[
                &["new", "-d", "-s", "base", "cat"],
                &["set", "-g", "update-environment", "MYVAR ABSENTVAR TEST_*"],
                &["show", "-g", "update-environment"],
            ],
        );
        runs.push(w.raw_env(
            &[
                "-L",
                &w.label,
                "-f",
                "/dev/null",
                "new",
                "-d",
                "-s",
                "two",
                "cat",
            ],
            &env,
        ));
        runs.push(w.raw_env(
            &[
                "-L",
                &w.label,
                "-f",
                "/dev/null",
                "new",
                "-d",
                "-E",
                "-s",
                "three",
                "cat",
            ],
            &env,
        ));
        runs.extend(steps(
            w,
            &[
                &["showenv", "-t", "two", "MYVAR"],
                &["showenv", "-t", "two", "TEST_GLOB"],
                &["showenv", "-t", "two", "ABSENTVAR"],
                &["showenv", "-t", "two", "OTHER"],
                &["showenv", "-t", "two", "DISPLAY"],
                &["showenv", "-t", "three"],
                &["showenv", "-t", "base", "MYVAR"],
            ],
        ));
        runs
    });
}

/// list-clients without clients prints nothing; `-O` is validated.
#[test]
fn list_clients_without_clients() {
    compare("list_clients_without_clients", |w| {
        steps(
            w,
            &[
                &["new", "-d", "-s", "a", "cat"],
                &["lsc"],
                &["list-clients", "-F", "#{client_name}"],
                &["list-clients", "-O", "bogus"],
                &["list-clients", "-t", "a"],
                &["list-clients", "-t", "nosuch"],
            ],
        )
    });
}

/// `-L label -f /dev/null args...` with HOME pinned, so `$HOME` expanded
/// into a binding at bind time prints the same on both sides.
fn run_home(w: &World, args: &[&str]) -> Run {
    let mut full = vec!["-L", &w.label, "-f", "/dev/null"];
    full.extend_from_slice(args);
    w.raw_env(&full, &[("HOME", "/home/vtmux-test")])
}

/// bind-key, unbind-key, list-keys over the server's tables
/// (cmd-bind-key.c, cmd-unbind-key.c, cmd-list-keys.c): the fresh default
/// list byte for byte, every flag, `-N` notes, tables created and removed,
/// the error texts, and a quiet failure that fails silently with exit 0.
#[test]
fn keys_bind_unbind_list() {
    compare("keys_bind_unbind_list", |w| {
        let mut out = vec![w.run(&["new", "-d", "-s", "a", "cat"])];
        for args in [
            &["lsk"][..],
            &["lsk", "-N"],
            &["bind", "-N", "note", "-r", "-T", "t1", "x", "display", "hi"],
            &["bind", "-n", "M-q"],
            &["bind", "q"],
            &["bind", "Foo", "x"],
            &["bind", "x", "nosuch"],
            &["bind"],
            &["unbind"],
            &["unbind", "-a", "x"],
            &["unbind", "-T", "nope", "x"],
            &["unbind", "-q", "-T", "nope", "x"],
            &["unbind", "-n", "Foo"],
            &["lsk", "-T", "t1"],
            &["lsk", "-T", "nope"],
            &["lsk", "C-b"],
            &["lsk", "-1", "-T", "prefix"],
            &["lsk", "-T", "prefix", "-N"],
            &["lsk", "-aN", "-T", "t1"],
            &["lsk", "-P", ">> ", "-N"],
            &[
                "bind", "-T", "t1", "{", "display", "a", ";", "display", "b", "}",
            ],
            &["bind", "-T", "t1", "y", "{ display a ; display 'b c' }"],
            &["lsk", "-T", "t1"],
            &["unbind", "-a", "-T", "t1"],
            &["lsk", "-T", "t1"],
            &["unbind", "-a", "-n"],
            &["lsk", "-T", "root"],
            &["unbind", "-a"],
            &["lsk"],
            &["switch-client", "-T", "prefix"],
            &["switch-client", "-t", "a"],
            &["send-prefix"],
            &["send-prefix", "-2"],
            &["send-keys", "-K", "x"],
        ] {
            out.push(run_home(w, args));
        }
        out
    });
}

/// The owner's bind and unbind lines through source-file: list-keys and
/// list-keys -N match after them (run-shell, display-popup and choose-tree
/// bindings are stored and printed; `$HOME` expands at bind time).
#[test]
fn keys_owner_config_binds() {
    compare("keys_owner_config_binds", |w| {
        let home = std::env::var_os("HOME").expect("HOME");
        let original =
            std::fs::read_to_string(PathBuf::from(home).join("dotfiles/tmux/.tmux.conf"))
                .unwrap_or_else(|_| {
                    include_str!("../../vtmux-core/src/keys/golden/owner_binds.conf").to_owned()
                });
        let mut text = original
            .lines()
            .filter(|l| l.starts_with("bind") || l.starts_with("unbind"))
            .collect::<Vec<_>>()
            .join("\n");
        text.push('\n');
        let config = w.dir().join("binds.conf");
        std::fs::write(&config, text).expect("write binds");
        vec![
            w.run(&["new", "-d", "-s", "a", "cat"]),
            run_home(w, &["source", config.to_str().expect("utf-8")]),
            run_home(w, &["lsk"]),
            run_home(w, &["lsk", "-N"]),
        ]
    });
}

/// A pane that prints `text` (printf format) each time it reads a line.
const ON_ENTER: &str = r#"while read x; do printf '%b' "$X"; done"#;

fn alert_pane(text: &str) -> String {
    format!("X='{text}'; {ON_ENTER}")
}

fn flags(w: &World) -> Run {
    w.run(&[
        "lsw",
        "-F",
        "#{window_index} [#{window_activity_flag}#{window_bell_flag}#{window_silence_flag}] #{window_flags} (#{session_alert}) (#{session_alerts}) #{session_activity_flag}#{session_bell_flag}#{session_silence_flag}",
    ])
}

/// alerts.c: output in a background window sets `#`, a BEL sets `!`,
/// `monitor-*` off sets nothing, selecting the window clears its flags,
/// `kill-session -C` clears them all, `next-window -a` jumps to a flagged
/// window (reviewer-28 two.py).
#[test]
fn alerts_activity_and_bell_flags() {
    compare("alerts_activity_and_bell_flags", |w| {
        let settle = || std::thread::sleep(Duration::from_millis(500));
        let bell = alert_pane(r"\007");
        let text = alert_pane("x");
        let mut out = vec![
            w.run(&[
                "new", "-d", "-s", "a", "-n", "w", "-x", "80", "-y", "24", "cat",
            ]),
            w.run(&["neww", "-d", "-n", "x", "sh", "-c", &text]),
            w.run(&["neww", "-d", "-n", "y", "sh", "-c", &bell]),
        ];
        settle();
        out.push(flags(w));
        // Nothing is monitored for activity yet (off by default); the BEL
        // alerts because monitor-bell is on.
        out.push(w.run(&["send-keys", "-t", "a:1", "Enter"]));
        out.push(w.run(&["send-keys", "-t", "a:2", "Enter"]));
        settle();
        out.push(flags(w));
        out.push(w.run(&["set", "-g", "monitor-activity", "on"]));
        settle();
        out.push(flags(w));
        out.push(w.run(&["send-keys", "-t", "a:1", "Enter"]));
        settle();
        out.push(flags(w));
        out.push(w.run(&[
            "display",
            "-p",
            "-t",
            "a:1",
            "[#{window_activity_flag}][#{window_flags}]",
        ]));
        out.push(w.run(&["next-window", "-a"]));
        out.push(w.run(&["display", "-p", "#{window_index}"]));
        out.push(w.run(&["next-window", "-a"]));
        out.push(w.run(&["display", "-p", "#{window_index}"]));
        out.push(flags(w));
        // The current window never gets its own flags.
        out.push(w.run(&["send-keys", "-t", "a:2", "Enter"]));
        settle();
        out.push(flags(w));
        out.push(w.run(&["select-window", "-t", "a:0"]));
        out.push(flags(w));
        out.push(w.run(&["send-keys", "-t", "a:1", "Enter"]));
        settle();
        out.push(flags(w));
        out.push(w.run(&["kill-session", "-C"]));
        out.push(flags(w));
        out.push(w.run(&["previous-window", "-a"]));
        // monitor-bell off: the BEL leaves no flag.
        out.push(w.run(&["set", "-g", "monitor-bell", "off"]));
        out.push(w.run(&["send-keys", "-t", "a:2", "Enter"]));
        settle();
        out.push(flags(w));
        out
    });
}

/// `bell-action`/`activity-action` decide which windows alert
/// (`alerts_action_applies`); the flag is set either way for a background
/// window, but only an applying action alerts.
#[test]
fn alerts_actions_apply_by_window() {
    compare("alerts_actions_apply_by_window", |w| {
        let settle = || std::thread::sleep(Duration::from_millis(500));
        let bell = alert_pane(r"\007");
        let mut out = vec![
            w.run(&[
                "new", "-d", "-s", "a", "-n", "w", "-x", "80", "-y", "24", "cat",
            ]),
            w.run(&["neww", "-d", "-n", "b", "sh", "-c", &bell]),
            w.run(&["set", "-g", "bell-action", "none"]),
            w.run(&["set", "-g", "monitor-activity", "on"]),
            w.run(&["set", "-g", "activity-action", "current"]),
        ];
        out.push(w.run(&["send-keys", "-t", "a:1", "Enter"]));
        settle();
        out.push(flags(w));
        out.push(w.run(&["select-window", "-t", "a:1"]));
        out.push(w.run(&["send-keys", "-t", "a:1", "Enter"]));
        settle();
        out.push(flags(w));
        out
    });
}

/// `monitor-silence`: a window that was output-active and then falls
/// quiet for that many seconds gets `~`; selecting it clears the flag, and
/// the timer starts again with the next output.
#[test]
fn alerts_silence_flag() {
    compare("alerts_silence_flag", |w| {
        let text = alert_pane("x");
        let mut out = vec![
            w.run(&[
                "new", "-d", "-s", "a", "-n", "w", "-x", "80", "-y", "24", "cat",
            ]),
            w.run(&["neww", "-d", "-n", "q", "sh", "-c", &text]),
            w.run(&["set", "-g", "monitor-bell", "off"]),
            w.run(&["setw", "-g", "monitor-silence", "1"]),
        ];
        out.push(flags(w));
        std::thread::sleep(Duration::from_millis(1700));
        out.push(flags(w));
        out.push(w.run(&["select-window", "-t", "a:1"]));
        out.push(flags(w));
        out.push(w.run(&["select-window", "-t", "a:0"]));
        std::thread::sleep(Duration::from_millis(1700));
        out.push(flags(w));
        out.push(w.run(&["setw", "-g", "monitor-silence", "0"]));
        out.push(w.run(&["select-window", "-t", "a:1"]));
        out.push(w.run(&["select-window", "-t", "a:0"]));
        std::thread::sleep(Duration::from_millis(1700));
        out.push(flags(w));
        out
    });
}

/// Paste buffers on the command line: raw bytes through `save-buffer -`
/// and `show-buffer`, `-a`, file round trip, eviction at `buffer-limit`
/// and the `-S`-less vis encoding of `paste-buffer` into a `cat -v` pane.
/// Reference: the same transcript on tmux 3.8.
#[test]
fn buffers_cli() {
    compare("buffers_cli", |w| {
        let mut out = vec![w.run(&["new", "-d", "-s", "a", "-x", "40", "-y", "5", "cat -v"])];
        out.push(w.run(&["set-buffer", "-b", "b", "x\ny\\z\u{1}\t"]));
        out.push(w.run(&["save-buffer", "-b", "b", "-"]));
        out.push(w.run(&["show-buffer", "-b", "b"]));
        out.push(w.run(&["save-buffer", "-b", "b", "f"]));
        out.push(w.run(&["save-buffer", "-a", "-b", "b", "f"]));
        out.push(w.run(&["load-buffer", "-b", "g", "f"]));
        out.push(w.run(&["list-buffers", "-O", "size", "-r"]));
        out.push(w.run(&["set-option", "-g", "buffer-limit", "2"]));
        for d in ["1", "2", "3"] {
            out.push(w.run(&["set-buffer", d]));
        }
        out.push(w.run(&["list-buffers", "-F", "#{buffer_name}=#{buffer_sample}"]));
        out.push(w.run(&["set-buffer", "-b", "p", "a\u{1b}[0m\u{7f}\nb"]));
        out.push(w.run(&["paste-buffer", "-b", "p", "-t", "a:0.0"]));
        // The first match is the tty's echo; wait for `cat -v`'s own line,
        // which follows the echoed `b`.
        w.wait_for_text("a:0.0", "ba^[[0m^?");
        out.push(w.run(&["capture-pane", "-p", "-t", "a:0.0"]));
        out
    });
}

/// D23 off-state: `vtmux-focus-ring` is invisible until set. Listings on a
/// fresh server are byte-identical to tmux's (`show -g`, `-gw`, `-s`,
/// `-A`, `-wA`, `-pA`), the window keeps its size, and prefixes of the name
/// keep tmux's errors. A config guarding it with `%if "#{vtmux_version}"`
/// loads cleanly in both. Then, vtmux only: a named lookup prints the
/// default, the guarded `set` took effect, and once set it is listed at
/// that scope.
#[test]
fn vtmux_options_invisible_while_unset() {
    compare("vtmux_options_invisible_while_unset", |w| {
        vec![
            w.run(&["new", "-d", "-s", "a", "-x", "80", "-y", "24", "cat"]),
            w.run(&["show", "-g"]),
            w.run(&["show", "-gw"]),
            w.run(&["show", "-s"]),
            w.run(&["show", "-A"]),
            w.run(&["show", "-wA"]),
            w.run(&["show", "-pA"]),
            w.run(&["display", "-p", "#{window_width}x#{window_height}"]),
            w.run(&["set", "-g", "vt", "on"]),
            w.run(&["show", "-g", "vtm"]),
            w.run(&["set", "-gw", "vtmux", "on"]),
            w.run(&["source", &guarded_config(w)]),
        ]
    });
    let w = World::new(&vtmux_bin());
    w.run(&["new", "-d", "-s", "a", "cat"]);
    let out = |args: &[&str]| w.run(args).stdout;
    w.run(&["source", &guarded_config(&w)]);
    assert_eq!(out(&["show", "-gv", "vtmux-focus-ring"]), "on\n");
    w.run(&["set", "-gwu", "vtmux-focus-ring"]);
    assert_eq!(out(&["show", "-gv", "vtmux-focus-ring"]), "off\n");
    assert_eq!(out(&["show", "-w", "vtmux-focus-ring"]), "");
    assert_eq!(out(&["display", "-p", "[#{vtmux-focus-ring}]"]), "[]\n");
    let r = w.run(&["set", "-gw", "vtmux-focus-ring", "on"]);
    assert_eq!((r.code, r.stderr.as_str()), (0, ""));
    assert!(out(&["show", "-gw"]).ends_with("vtmux-focus-ring on\n"));
    assert_eq!(out(&["show", "-wv", "vtmux-focus-ring"]), "");
    assert_eq!(out(&["show", "-wAv", "vtmux-focus-ring"]), "on\n");
    assert_eq!(out(&["display", "-p", "[#{vtmux-focus-ring}]"]), "[1]\n");
}

/// The D23 config guard: real tmux skips the vtmux option.
fn guarded_config(w: &World) -> String {
    let path = w.dir().join("guard.conf");
    std::fs::write(
        &path,
        "set -g pane-border-lines heavy\n%if \"#{vtmux_version}\"\nset -g vtmux-focus-ring on\n%endif\n",
    )
    .expect("write");
    path.to_str().expect("utf-8").to_owned()
}

impl World {
    /// `run` with `input` on the client's stdin.
    fn run_stdin(&self, args: &[&str], input: &[u8]) -> Run {
        use std::io::Write as _;
        let mut child = Command::new(&self.bin)
            .args(["-L", &self.label, "-f", "/dev/null"])
            .args(args)
            .current_dir(self.dir())
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env("TMUX_TMPDIR", self.dir())
            .env("SHELL", "/bin/sh")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn binary");
        child.stdin.take().unwrap().write_all(input).unwrap();
        let out = child.wait_with_output().expect("wait");
        Run {
            stdout: self.normalise(&String::from_utf8_lossy(&out.stdout)),
            stderr: self.normalise(&String::from_utf8_lossy(&out.stderr)),
            code: out.status.code().unwrap_or(-1),
        }
    }

    /// `run` with raw (possibly non-UTF-8) command words.
    fn run_bytes(&self, args: &[&[u8]]) -> Run {
        self.run_io(args, None)
    }

    /// `run_bytes` with `input` on stdin. Stdout and stderr are compared as
    /// escaped bytes, so a lossy decode cannot hide a difference; the time
    /// and command number of a control guard line (server-wide counters)
    /// are blanked.
    fn run_io(&self, args: &[&[u8]], input: Option<&[u8]>) -> Run {
        use std::io::Write as _;
        use std::os::unix::ffi::OsStrExt;
        let mut full: Vec<&std::ffi::OsStr> = ["-L", &self.label, "-f", "/dev/null"]
            .into_iter()
            .map(std::ffi::OsStr::new)
            .collect();
        full.extend(args.iter().map(|a| std::ffi::OsStr::from_bytes(a)));
        let mut child = Command::new(&self.bin)
            .args(full)
            .current_dir(self.dir())
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env("TMUX_TMPDIR", self.dir())
            .env("SHELL", "/bin/sh")
            .stdin(if input.is_some() {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn binary");
        if let Some(input) = input {
            child.stdin.take().unwrap().write_all(input).unwrap();
        }
        let out = child.wait_with_output().expect("wait");
        let guards = |s: String| -> String {
            s.split_inclusive("\\n")
                .map(|l| {
                    let mut f = l.split(' ');
                    match (f.next(), f.next(), f.next(), f.next()) {
                        (
                            Some(k @ ("%begin" | "%end" | "%error")),
                            Some(_),
                            Some(_),
                            Some(rest),
                        ) => format!("{k} T N {rest}"),
                        _ => l.to_owned(),
                    }
                })
                .collect()
        };
        Run {
            stdout: self.normalise(&guards(out.stdout.escape_ascii().to_string())),
            stderr: self.normalise(&out.stderr.escape_ascii().to_string()),
            code: out.status.code().unwrap_or(-1),
        }
    }
}

/// `load-buffer -` reads the command client's stdin (file.c `file_read`
/// of `-`: `MSG_READ_OPEN` with `STDIN_FILENO`); empty stdin sets
/// nothing; `save-buffer` to a missing directory and `load-buffer` of a
/// missing file fail after the I/O. Reference: the same transcript.
#[test]
fn buffers_stdin_and_file_errors() {
    compare("buffers_stdin_and_file_errors", |w| {
        vec![
            w.run(&["new", "-d", "-s", "a", "cat"]),
            w.run_stdin(&["load-buffer", "-b", "b", "-"], b"x"),
            w.run_stdin(&["load-buffer", "-b", "c", "-"], b""),
            w.run_stdin(&["load-buffer", "-"], b"line\n\xff\x00tail"),
            w.run(&["list-buffers"]),
            w.run(&[
                "save-buffer",
                "-b",
                "b",
                "nodir/f",
                ";",
                "display",
                "-p",
                "after",
            ]),
            w.run(&["load-buffer", "nosuch", ";", "display", "-p", "after"]),
            w.run(&["load-buffer", "."]),
        ]
    });
}

/// `set-buffer` stores the argv bytes as given (cmd-set-buffer.c:111-123):
/// invalid UTF-8 is not replaced (reviewer-36: `\xff\x80` became EF BF BD
/// twice). Reference: `buffer0: 14 bytes`, `showb` returns the bytes.
#[test]
fn set_buffer_keeps_non_utf8_bytes() {
    compare("set_buffer_keeps_non_utf8_bytes", |w| {
        let data = b"a\tb\x01c\x1b[31m\xc3\xa9\xff\x80";
        vec![
            w.run(&["new", "-d", "-s", "a", "cat"]),
            w.run_bytes(&[b"set-buffer", data]),
            w.run_bytes(&[b"set-buffer", b"-a", b"-b", b"n", b"\xfe;"]),
            w.run_bytes(&[b"list-buffers"]),
            w.run_bytes(&[b"show-buffer"]),
            w.run_bytes(&[b"save-buffer", b"-b", b"n", b"-"]),
        ]
    });
}

/// A `save-buffer` to a FIFO whose reader is slow must not hold up other
/// commands: tmux has the command client write it (file.c:611, 635), and
/// so does vtmux. 100000 bytes is more than the pipe holds, so the writer
/// waits until the test drains the FIFO; meanwhile `display` answers.
#[test]
fn save_buffer_to_a_slow_fifo_does_not_stall_the_server() {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt;
    compare(
        "save_buffer_to_a_slow_fifo_does_not_stall_the_server",
        |w| {
            let fifo = w.dir().join("fifo");
            let big = w.dir().join("big");
            std::fs::write(&big, vec![b'z'; 100_000]).unwrap();
            let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
            assert!(status.success());
            let mut out = vec![
                w.run(&["new", "-d", "-s", "a", "cat"]),
                w.run(&["load-buffer", "-b", "w", big.to_str().unwrap()]),
            ];
            // The read end, open but not read yet.
            let mut reader = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc_o_nonblock())
                .open(&fifo)
                .unwrap();
            let writer = w.spawn(&["save-buffer", "-b", "w", fifo.to_str().unwrap()]);
            std::thread::sleep(Duration::from_millis(300));
            let start = Instant::now();
            out.push(w.run(&["display", "-p", "alive"]));
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "display waited for the FIFO ({})",
                w.bin.display()
            );
            let mut got = Vec::new();
            let mut buf = [0u8; 8192];
            let deadline = Instant::now() + Duration::from_secs(5);
            while got.len() < 100_000 && Instant::now() < deadline {
                match reader.read(&mut buf) {
                    Ok(0) => std::thread::sleep(Duration::from_millis(10)),
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("{e}"),
                }
            }
            let done = writer.wait_with_output().unwrap();
            out.push(Run {
                stdout: format!("{} bytes", got.len()),
                stderr: String::from_utf8_lossy(&done.stderr).into_owned(),
                code: done.status.code().unwrap_or(-1),
            });
            out
        },
    );
}

/// `O_NONBLOCK` from the platform, never hard-coded.
fn libc_o_nonblock() -> i32 {
    rustix::fs::OFlags::NONBLOCK.bits().cast_signed()
}

/// `load-buffer` of a FIFO opens it `O_NONBLOCK` and waits for it to poll
/// readable (file.c:808-812), so a writer that connects late is read to
/// EOF; a blocking `read(2)` would see EOF at once, set no buffer and
/// leave the writer without a reader. Reference: the writer succeeds, the
/// buffer is `late:10`.
#[test]
// Linux only: on macOS a FIFO with no writer yet polls as hung up, so the
// wait-for-writer path differs and the test hung CI (task macos_attach_parity).
#[cfg_attr(not(target_os = "linux"), ignore = "macos_attach_parity")]
fn load_buffer_of_a_fifo_waits_for_the_writer() {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    compare("load_buffer_of_a_fifo_waits_for_the_writer", |w| {
        let fifo = w.dir().join("ff");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let mut out = vec![w.run(&["new", "-d", "-s", "a", "cat"])];
        let loader = w.spawn(&["load-buffer", "-b", "late", fifo.to_str().unwrap()]);
        std::thread::sleep(Duration::from_millis(500));
        // A non-blocking open fails with ENXIO while nobody reads.
        let deadline = Instant::now() + Duration::from_secs(2);
        let writer = loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc_o_nonblock())
                .open(&fifo)
            {
                Err(e) if Instant::now() < deadline && e.raw_os_error() == Some(6) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                other => break other,
            }
        };
        let wrote = match writer {
            Ok(mut f) => f.write_all(b"late-data\n").map(|()| drop(f)).is_ok(),
            Err(_) => false,
        };
        out.push(Run {
            stdout: format!("writer ok: {wrote}"),
            stderr: String::new(),
            code: 0,
        });
        let done = loader.wait_with_output().unwrap();
        out.push(Run {
            stdout: String::from_utf8_lossy(&done.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&done.stderr).into_owned(),
            code: done.status.code().unwrap_or(-1),
        });
        out.push(w.run(&["list-buffers", "-F", "#{buffer_name}:#{buffer_size}"]));
        out
    });
}

/// A command client's stdin and stdout can each be used once (file.c
/// `file_read_open` / `file_write_open` close the original descriptor,
/// client.c:691-701): a second `load-buffer -`, `show-buffer` or
/// `save-buffer -` fails with `Bad file descriptor: -`, and after a
/// `save-buffer -` a print goes nowhere.
#[test]
fn stdin_and_stdout_are_usable_once_per_client() {
    compare("stdin_and_stdout_are_usable_once_per_client", |w| {
        vec![
            w.run(&["new", "-d", "-s", "a", "cat"]),
            w.run_stdin(
                &[
                    "load-buffer",
                    "-b",
                    "x",
                    "-",
                    ";",
                    "load-buffer",
                    "-b",
                    "y",
                    "-",
                ],
                b"12",
            ),
            w.run(&["set-buffer", "-b", "a", "zz"]),
            w.run(&["show-buffer", "-b", "a", ";", "show-buffer", "-b", "a"]),
            w.run(&["save-buffer", "-b", "a", "-", ";", "display", "-p", "b"]),
            w.run(&[
                "save-buffer",
                "-b",
                "a",
                "-",
                ";",
                "save-buffer",
                "-b",
                "a",
                "-",
            ]),
            w.run(&["display", "-p", "-", ";", "show-buffer", "-b", "a"]),
            w.run(&["list-buffers", "-F", "#{buffer_name}:#{buffer_size}"]),
        ]
    });
}

/// File I/O for a command inside `source-file` still goes to the command
/// client (file.c:321, 395 only look at the client's flags): `load-buffer
/// -` reads its stdin and `save-buffer` writes, and the file's later lines
/// run after the I/O. A second `-` in the file fails as it does outside.
#[test]
fn buffer_file_io_inside_source_file_uses_the_client() {
    compare("buffer_file_io_inside_source_file_uses_the_client", |w| {
        let out = w.dir().join("srcout");
        let conf = w.dir().join("src.conf");
        let text = format!(
            "loadb -b src -\nsaveb -b src {}\nshowb -b src\nloadb -b again -\n",
            out.display()
        );
        std::fs::write(&conf, text).unwrap();
        let again = w.dir().join("again.conf");
        std::fs::write(&again, "display -p second\n").unwrap();
        let mut runs = vec![
            w.run(&["new", "-d", "-s", "a", "cat"]),
            w.run_stdin(&["source-file", conf.to_str().unwrap()], b"via-source"),
            w.run(&["list-buffers", "-F", "#{buffer_name}:#{buffer_size}"]),
        ];
        runs.push(Run {
            stdout: std::fs::read_to_string(&out).unwrap_or_default(),
            stderr: String::new(),
            code: 0,
        });
        // The rest of the batch runs after a parked source-file.
        let once = w.dir().join("once.conf");
        std::fs::write(&once, "loadb -b q -\ndisplay -p inside\n").unwrap();
        runs.push(w.run_stdin(
            &[
                "source-file",
                once.to_str().unwrap(),
                ";",
                "display",
                "-p",
                "outside",
            ],
            b"q",
        ));
        runs.push(w.run(&[
            "source-file",
            again.to_str().unwrap(),
            ";",
            "display",
            "-p",
            "after",
        ]));
        runs
    });
}

/// after-* hooks (cmd-queue.c `cmdq_insert_hook`, hooks.c `hooks_insert`):
/// the hook's commands run right after the command, before the next one in
/// its list, with the arguments and flags as `hook_*` formats and the
/// command's target as current state; commands a hook runs fire no hooks;
/// command-error on failure; a failing hook command does not fail the
/// client; `set-hook -R` and `-E` run a hook now; fire counts.
#[test]
#[allow(clippy::too_many_lines)] // one scripted sequence
fn hooks_after_commands() {
    compare("hooks_after_commands", |w| {
        let mut out = vec![w.run(&["new", "-d", "-s", "a", "-n", "w", "cat"])];
        let mut run = |argv: &[&str]| out.push(w.run(argv));
        run(&[
            "set-hook",
            "-g",
            "after-rename-window",
            "display -p \"#{hook}|#{hook_arguments}|#{hook_argument_0}|#{hook_flag_t}|#{session_name}:#{window_index}|#{command}\"",
        ]);
        run(&["rename-window", "-t", "a:0", "x"]);
        run(&[
            "set-hook",
            "-ga",
            "after-rename-window",
            "set -g @o \"#{@o}H\"",
        ]);
        run(&["set", "-g", "@o", ""]);
        run(&[
            "rename-window",
            "-t",
            "a:0",
            "y",
            ";",
            "set",
            "-gF",
            "@o",
            "#{@o}N",
        ]);
        run(&["show", "-gv", "@o"]);
        run(&["set-hook", "-g", "after-rename-window", "nosuchcmd"]);
        run(&["rename-window", "-t", "a:0", "z"]);
        run(&[
            "set-hook",
            "-g",
            "after-rename-window",
            "rename-window -t a:0 inner ; display -p \"#{window_name}\"",
        ]);
        run(&["rename-window", "-t", "a:0", "outer"]);
        run(&["set-hook", "-R", "-t", "a", "after-rename-window"]);
        run(&["set-hook", "-gu", "after-rename-window"]);
        run(&[
            "set-hook",
            "-g",
            "command-error",
            "display -p \"E #{hook_arguments}\"",
        ]);
        run(&["rename-window", "-t", "nosuch:0", "q"]);
        run(&["set-hook", "-gu", "command-error"]);
        run(&[
            "set-hook",
            "-g",
            "after-split-window",
            "display -p \"#{hook_arguments}|#{hook_flag_e}|#{hook_flag_e_0}|#{hook_flag_e_1}|#{pane_index}\"",
        ]);
        run(&[
            "split-window",
            "-d",
            "-t",
            "a:0",
            "-e",
            "A=1",
            "-e",
            "B=2",
            "cat",
        ]);
        run(&[
            "set-hook",
            "-g",
            "after-select-pane",
            "display -p \"sp #{hook_pane}|#{pane_index}|#{hook_flag_t}\"",
        ]);
        run(&["selectp", "-t", "a:0.1"]);
        run(&["selectp", "-t", "a:0.1"]);
        run(&[
            "set-hook",
            "-g",
            "after-select-window",
            "display -p \"sw #{window_index}\"",
        ]);
        run(&["neww", "-d", "-t", "a:3", "cat"]);
        run(&["selectw", "-t", "a:3"]);
        run(&[
            "set-hook",
            "-g",
            "after-new-window",
            "display -p \"nw #{window_index}\"",
        ]);
        run(&["neww", "-d", "-t", "a:5", "cat"]);
        run(&[
            "set-hook",
            "-g",
            "after-new-session",
            "display -p \"ns #{session_name}\"",
        ]);
        run(&["new", "-d", "-s", "b", "cat"]);
        run(&[
            "set-hook",
            "-g",
            "@u",
            "display -p \"u #{hook} #{session_name}\"",
        ]);
        run(&["set-hook", "-R", "-t", "b", "@u"]);
        run(&["set-hook", "-E", "-t", "b", "@u"]);
        run(&["set-hook", "-E", "window-renamed"]);
        run(&[
            "show-hooks",
            "-gF",
            "#{option_name}:#{hook_fire_count}",
            "after-select-pane",
        ]);
        run(&[
            "show-hooks",
            "-gF",
            "#{option_name}:#{hook_fire_count}",
            "@u",
        ]);
        run(&["wait-for", "-E", "bogus"]);
        out
    });
}

/// pane-exited's `exit_signal` is `sig2name` of the signal (server-fn.c:
/// 35-56, tmux.c:327-338): the number on glibc, which has no
/// `sys_signame`, the lower-case name on the BSDs and macOS. Compared
/// against the reference on this host, not an oracle trace, because the
/// value depends on the platform.
#[test]
fn hooks_exit_signal() {
    compare("hooks_exit_signal", |w| {
        let mut out = vec![w.run(&["new", "-d", "-s", "a", "-n", "w", "cat"])];
        let mut run = |argv: &[&str]| out.push(w.run(argv));
        run(&["set", "-g", "@log", ""]);
        run(&[
            "set-hook",
            "-g",
            "pane-exited",
            "set -agF @log '|#{hook_exit_signal}:#{hook_exit_status}:#{hook_exit_success}'",
        ]);
        run(&["set-hook", "-ga", "pane-exited", "wait-for -S exited"]);
        for program in ["kill -TERM $$", "kill -KILL $$", "kill -HUP $$", "exit 0"] {
            let program = format!("sh -c '{program}'");
            run(&["neww", "-d", "-n", "x", &program]);
            run(&["wait-for", "exited"]);
        }
        run(&["show", "-gv", "@log"]);
        run(&["kill-server"]);
        out
    });
}

/// Words that are not UTF-8 are bytes end to end, as tmux's C strings are
/// (reviewer-44, `low_non_utf_8_bytes_in_a_command_string`): argv, `run -C`,
/// `source-file`, a control client's stdin, `if-shell`, and the `\377` /
/// `\U` escapes of the config grammar all print the raw bytes.
#[test]
fn non_utf8_bytes_in_command_strings() {
    compare("non_utf8_bytes_in_command_strings", |w| {
        // reference: T display -p "c$(printf '\351')" | od -c  -> c 351
        let file = w.dir().join("f");
        std::fs::write(
            &file,
            b"display -p \"c\\351\"\ndisplay -p \\U00110000\ndisplay -p \\U7fffffff\n\
              display -p \\U80000000\nA=x\\377y\ndisplay -p \"$A\"\n\
              %if \"\\377\"\ndisplay -p yes\n%endif\nrun -C \"display -p \\\"z\\351\\\"\"\n",
        )
        .unwrap();
        let f = file.to_str().unwrap().as_bytes().to_vec();
        vec![
            w.run(&["new", "-d", "-s", "s", "cat"]),
            w.run_bytes(&[b"display", b"-p", b"c\xe9"]),
            w.run_bytes(&[b"run", b"-C", b"display -p \"c\\351\""]),
            w.run_bytes(&[b"run", b"-C", b"display -p \"c\xe9\""]),
            w.run_bytes(&[b"source", &f]),
            w.run_bytes(&[b"source", b"-nv", &f]),
            w.run_bytes(&[b"if", b"-F", b"1", b"display -p x\xe9"]),
            w.run_bytes(&[b"if", b"-F", b"x\xe9", b"display -p yes"]),
            w.run_bytes(&[b"display", b"-p", b"\\U00110000"]),
            // A bad flag byte is reported as the byte.
            w.run_bytes(&[b"display", b"-\xe9"]),
            // Printed back as the command was written: octal for each byte.
            w.run_bytes(&[b"bind", b"-T", b"foo", b"x", b"display", b"a\xff"]),
            w.run_bytes(&[b"list-keys", b"-T", b"foo"]),
            w.run_io(
                &[b"-C", b"attach", b"-t", b"s"],
                Some(b"display -p ok\ndisplay -p c\xe9\nnosuchcmd\n"),
            ),
        ]
    });
}

/// Option values, user options, environment and buffers hold raw bytes, and
/// formats see them as C strings: `n` counts bytes, `w` and `=` skip bytes
/// that are not UTF-8, `p` pads by `utf8_cstrwidth`, `.` and `m/r` never
/// match one (reviewer-40, `low_non_utf_8_window_buffer_names_are`).
#[test]
fn non_utf8_bytes_through_options_and_formats() {
    compare("non_utf8_bytes_through_options_and_formats", |w| {
        let o: &[u8] = b"a\xffb\xc3\xa9c";
        let mut steps = vec![w.run(&["new", "-d", "-s", "s", "cat"])];
        steps.push(w.run_bytes(&[b"display", b"-p", o]));
        steps.push(w.run_bytes(&[b"set", b"-g", b"@o", o]));
        steps.push(w.run_bytes(&[b"show", b"-g", b"@o"]));
        steps.push(w.run_bytes(&[b"show", b"-gv", b"@o"]));
        steps.push(w.run_bytes(&[b"set-environment", b"-g", b"FOO", o]));
        steps.push(w.run_bytes(&[b"show-environment", b"-g", b"FOO"]));
        steps.push(w.run_bytes(&[b"display", b"-p", b"#{FOO}"]));
        steps.push(w.run_bytes(&[b"set-buffer", o]));
        steps.push(w.run_bytes(&[b"list-buffers", b"-F", b"#{buffer_sample}|#{buffer_full}"]));
        // reference: set -gF @f '<fmt>'; show -gv @f
        for f in [
            "#{=3:@o}",
            "#{=-3:@o}",
            "#{=/3/..:@o}",
            "#{p8:@o}|",
            "#{p-8:@o}|",
            "#{w:@o}",
            "#{n:@o}",
            "#{b:@o}",
            "#{q/e:@o}",
            "#{s/b/Z/:@o}",
            "#{s/./Z/:@o}",
            "#{m/r:a.b,#{@o}}",
            "#{m:a?b*,#{@o}}",
            "#{R:#{@o},2}",
            "#{==:#{@o},#{@o}}",
            "#{?#{@o},Y,N}",
        ] {
            steps.push(w.run(&["set", "-gF", "@f", f]));
            steps.push(w.run_bytes(&[b"show", b"-gv", b"@f"]));
        }
        // Truncated, padded and counted characters around lone lead bytes.
        for v in [
            &b"\xe4"[..],
            b"\xe4\xb8",
            b"\xe4\xb8x",
            b"a\xe4\xb8x",
            b"\xe4\xb8\xad",
            b"\xf0\x9f\x98",
            b"\x80",
            b"\xc0\x80",
        ] {
            steps.push(w.run_bytes(&[b"set", b"-g", b"@o", v]));
            for f in [
                &b"#{n:@o}"[..],
                b"#{w:@o}",
                b"#{=2:@o}",
                b"#{=-2:@o}",
                b"#{p4:@o}|",
                b"#{s/./x/:@o}",
                b"#{m:?,#{@o}}",
                b"#{m/r:^.$,#{@o}}",
            ] {
                steps.push(w.run_bytes(&[b"display", b"-p", f]));
            }
        }
        steps
    });
}

/// Names must be valid UTF-8 (tmux.c `check_name`): `new-window -n`,
/// `rename-window`, `rename-session`, `new-session -s`, `set-buffer -b` and
/// `-n` fail with `invalid ... name` and the raw bytes; nothing is created.
#[test]
fn non_utf8_names_are_rejected() {
    compare("non_utf8_names_are_rejected", |w| {
        let bad: &[u8] = b"n\xff";
        vec![
            w.run(&["new", "-d", "-s", "s", "-n", "w", "cat"]),
            w.run_bytes(&[b"new-window", b"-d", b"-n", bad]),
            w.run_bytes(&[b"new-window", b"-d", b"-n", b"a\xff\xc3\xa9"]),
            w.run_bytes(&[b"rename-window", bad]),
            w.run_bytes(&[b"rename-session", bad]),
            w.run_bytes(&[b"new-session", b"-d", b"-s", bad]),
            w.run_bytes(&[b"set-buffer", b"-b", bad, b"v"]),
            w.run_bytes(&[b"set-buffer", b"-b", b"ok", b"v"]),
            w.run_bytes(&[b"set-buffer", b"-b", b"ok", b"-n", bad]),
            w.run(&["list-windows", "-F", "#{window_name}"]),
            w.run(&["list-sessions", "-F", "#{session_name}"]),
            w.run(&["list-buffers", "-F", "#{buffer_name}"]),
        ]
    });
}

/// Valid UTF-8 in plane 16 (U+10FF80..=U+10FFFF, private use) is an ordinary
/// character: names take it, `show` and `source -v` print it, `.` and `?`
/// match it, `w`, `=N` and `p` count it, `\U0010FFFF` makes it (reviewer-52,
/// `medium_rawstr_steals_valid_utf_8_u`: 91d2579 read it as four bytes).
#[test]
fn plane16_characters_are_ordinary_text() {
    compare("plane16_characters_are_ordinary_text", |w| {
        let p = "\u{10ff80}";
        let q = "\u{10ffff}";
        let apb = format!("a{p}b");
        let file = w.dir().join("f");
        std::fs::write(
            &file,
            "display -p \"a\\U0010FF80b\"\nset -g @u \"\\U0010FFFF\"\nshow -g @u\n",
        )
        .unwrap();
        let f = file.to_str().unwrap();
        // A fixed name for window 0: its automatic name races (D-012).
        let mut steps = vec![w.run(&["new", "-d", "-s", "s", "-n", "w", "cat"])];
        for n in [p, q, &apb] {
            steps.push(w.run(&["new-window", "-d", "-n", n]));
            steps.push(w.run(&["setb", "-b", n, "v"]));
            steps.push(w.run(&["set", "-g", "@p", n]));
            steps.push(w.run_bytes(&[b"show", b"-g", b"@p"]));
            steps.push(w.run_bytes(&[b"display", b"-p", b"#{w:@p}|#{n:@p}|#{=2:@p}|#{p4:@p}|"]));
            steps.push(w.run_bytes(&[
                b"display",
                b"-p",
                b"#{s/./Z/:@p}|#{s/x*/-/:@p}|#{m/r:^a.b$,#{@p}}|#{m:a?b,#{@p}}|#{q:@p}",
            ]));
        }
        steps.push(w.run(&["rename-session", p]));
        steps.push(w.run_bytes(&[b"source", f.as_bytes()]));
        steps.push(w.run_bytes(&[b"source", b"-nv", f.as_bytes()]));
        steps.push(w.run_bytes(&[b"bind", b"-T", b"foo", b"x", b"display", apb.as_bytes()]));
        steps.push(w.run_bytes(&[b"list-keys", b"-T", b"foo"]));
        steps.push(w.run(&["lsw", "-F", "#{window_index}:#{window_name}"]));
        steps.push(w.run(&["lsb", "-F", "#{buffer_name}"]));
        steps.push(w.run(&["ls", "-F", "#{session_name}"]));
        steps
    });
}

/// `send-keys` writes what `utf8_fromcstr` makes of the argument: a byte
/// that is not UTF-8 goes to the pane as it is, plane-16 characters as
/// their four bytes (reviewer-52: 91d2579 wrote U+FFFD per byte).
#[test]
fn send_keys_passes_bytes_through() {
    compare("send_keys_passes_bytes_through", |w| {
        let got = w.dir().join("got");
        let cmd = format!("stty raw -echo; cat > {}", got.display());
        let mut steps = vec![w.run(&["new", "-d", "-s", "s", "-x", "60", "-y", "10", &cmd])];
        // The pane's shell must be in raw mode before anything is sent.
        std::thread::sleep(Duration::from_millis(500));
        let args: [&[&[u8]]; 7] = [
            &[b"-l", b"c\xe9d"],
            &[b"-l", b"a\xffb\xc3\xa9"],
            &[b"a\xffb"],
            &[b"-l", "\u{10ff80}".as_bytes()],
            &["\u{10ffff}".as_bytes()],
            &[b"-l", b"\x80"],
            &[b"-H", b"ff"],
        ];
        for a in args {
            let mut full: Vec<&[u8]> = vec![b"send-keys", b"-t", b"s"];
            full.extend_from_slice(a);
            steps.push(w.run_bytes(&full));
            steps.push(w.run_bytes(&[b"send-keys", b"-t", b"s", b"-l", b"|"]));
        }
        steps.push(w.run_bytes(&[b"send-keys", b"-t", b"s", b"-l", b"END"]));
        let start = Instant::now();
        let mut data = Vec::new();
        while start.elapsed() < Duration::from_secs(5) {
            data = std::fs::read(&got).unwrap_or_default();
            if data.ends_with(b"END") {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        steps.push(Run {
            stdout: data.escape_ascii().to_string(),
            stderr: String::new(),
            code: 0,
        });
        steps
    });
}

/// In a parsed command string tmux reads each byte as a signed `char`, so
/// 0xff is `EOF` (cmd-parse.y `yylex_getc1`): it ends the word, the rest is
/// another word, and a line holding one fails as `too many arguments`.
/// `run -C`, `if-shell`, `bind`, `set-hook`, `source-file` and control
/// lines all behave so; a startup `-f` file (`getc`) and argv keep the byte
/// (reviewer-52, `low_0xff_in_a_parsed_command_string_is`).
#[test]
fn byte_0xff_ends_a_parsed_command_string() {
    compare("byte_0xff_ends_a_parsed_command_string", |w| {
        let cfg = w.dir().join("cfg");
        std::fs::write(&cfg, b"set -g @a x\xffy\nset -g @b \"p\xffq\"\n").unwrap();
        let f = w.dir().join("f");
        std::fs::write(&f, b"display -p a\xffb\ndisplay -p next\n").unwrap();
        let g = w.dir().join("g");
        std::fs::write(&g, b"display -p \"a\xffb\"\ndisplay -p \"c\\377d\"\n").unwrap();
        let (cfg, f, g) = (
            cfg.to_str().unwrap().to_owned(),
            f.to_str().unwrap().to_owned(),
            g.to_str().unwrap().to_owned(),
        );
        let label = w.label.clone();
        vec![
            // `-f` replaces /dev/null: it is the startup config.
            w.raw(&["-L", &label, "-f", &cfg, "new", "-d", "-s", "s", "cat"]),
            w.run(&["show", "-g", "@a"]),
            w.run(&["show", "-g", "@b"]),
            w.run_bytes(&[b"display", b"-p", b"u\xffv"]),
            w.run_bytes(&[b"run", b"-C", b"display -p \"q\xffr\""]),
            w.run_bytes(&[b"run", b"-C", b"display -p a\xff"]),
            w.run_bytes(&[b"run", b"-C", b"\xffdisplay -p a"]),
            w.run_bytes(&[b"run", b"-C", b"display -p a\xff\ndisplay -p b"]),
            w.run_bytes(&[b"run", b"-C", b"display -p a\xfeb"]),
            w.run_bytes(&[b"bind", b"-T", b"foo", b"x", b"display -p a\xffb"]),
            w.run_bytes(&[b"if", b"true", b"display -p 'a\xffb'"]),
            w.run_bytes(&[
                b"set-hook",
                b"-g",
                b"after-new-window",
                b"display -p h\xffi",
            ]),
            w.run(&["source", &f]),
            w.run(&["source", &g]),
            w.run_io(
                &[b"-C", b"attach", b"-t", b"s"],
                Some(b"set -g @c a\xffb\nshow -gv @c\n"),
            ),
        ]
    });
}

/// A test that panics mid-way must not leave its server behind: `World`'s
/// drop records the server pid, runs kill-server and SIGKILLs a survivor.
#[test]
fn world_drop_reaps_the_server_on_panic() {
    let (_keep, as_tmux) = vtmux_as_tmux();
    let pid = std::sync::Mutex::new(None);
    let caught = std::panic::catch_unwind(|| {
        let w = World::new(&as_tmux);
        w.run(&["new", "-d", "-s", "a", "cat"]);
        *pid.lock().unwrap() = common::parse_pid(&w.run(&["display", "-p", "#{pid}"]).stdout);
        panic!("deliberate");
    });
    assert!(caught.is_err());
    let pid = pid.lock().unwrap().expect("server pid");
    assert!(
        rustix::process::test_kill_process(pid).is_err(),
        "server {pid:?} survived a panicking test"
    );
}

/// The message log without its volatile parts: the `HH:MM: ` prefix goes and
/// `client-PID` becomes `client-N`.
fn log_lines(run: &Run) -> Run {
    let mut stdout = String::new();
    for line in run.stdout.lines() {
        let mut text = line.get(7..).unwrap_or(line).to_owned();
        while let Some(at) = text.find("client-") {
            let digits = text[at + 7..]
                .bytes()
                .take_while(u8::is_ascii_digit)
                .count();
            text.replace_range(at + 7..at + 7 + digits, "N");
            text.replace_range(at..at + 7, "client_");
        }
        stdout.push_str(&text.replace("client_", "client-"));
        stdout.push('\n');
    }
    Run {
        stdout,
        stderr: run.stderr.clone(),
        code: run.code,
    }
}

/// `list-commands` (cmd-list-commands.c:40-100) against the reference: the
/// whole table, one command by name and by alias, `-F` with the three
/// variables, an empty expansion (no line), a lookup error and an ambiguous
/// prefix.
/// Reference: `tmux list-commands [-F fmt] [cmd]` with an isolated server.
#[test]
fn list_commands_table_formats_and_errors() {
    compare("list_commands_table_formats_and_errors", |w| {
        vec![
            w.run(&["new", "-d", "-s", "a", "cat"]),
            w.run(&["list-commands"]),
            w.run(&["lscm", "kill-server"]),
            w.run(&["list-commands", "lscm"]),
            w.run(&["list-commands", "new-session"]),
            w.run(&[
                "list-commands",
                "-F",
                "#{command_list_name}|#{command_list_alias}|#{command_list_usage}",
                "list-commands",
            ]),
            w.run(&["list-commands", "-F", "#{command_list_name}", "show-env"]),
            w.run(&["list-commands", "-F", "", "list-commands"]),
            w.run(&[
                "list-commands",
                "-F",
                "#{?command_list_alias,A,-}",
                "kill-server",
            ]),
            w.run(&["list-commands", "-F", "#{command_list_name}"]),
            w.run(&["list-commands", "nosuchcommand"]),
            w.run(&["list-commands", "list"]),
            w.run(&["list-commands", "a", "b"]),
        ]
    });
}

/// `show-messages` (cmd-show-messages.c:77-110, cmd-queue.c:534-567 and
/// 850-886, server.c:539-566): every command fired once the config has run
/// is logged `client-PID command: <cmd_print>`, an error for a client with
/// no session `client-PID message: <error>`, newest first; `-t` and `-J`
/// and `-T` with nothing to list print nothing; `message-limit` trims.
/// Reference: the same commands on the isolated reference server.
#[test]
fn show_messages_log_flags_and_limit() {
    compare("show_messages_log_flags_and_limit", |w| {
        let mut out = vec![
            w.run(&["new", "-d", "-s", "a", "cat"]),
            w.run(&[
                "new-window",
                "-d",
                "-n",
                "two words",
                ";",
                "set",
                "-g",
                "@x",
                "a b",
            ]),
            w.run(&["display-message", "-p", "#{session_name}"]),
            w.run(&["nosuchcommand"]),
            w.run(&["kill-pane", "-t", "nosuch"]),
            w.run(&["send-keys", "-t", "a", "-l", "it's \"q\""]),
        ];
        out.push(log_lines(&w.run(&["show-messages"])));
        out.push(log_lines(&w.run(&["showmsgs", "-t", "nosuch"])));
        out.push(w.run(&["show-messages", "-J"]));
        out.push(w.run(&["show-messages", "-T"]));
        out.push(w.run(&["show-messages", "-JT"]));
        out.push(w.run(&["set", "-g", "message-limit", "3"]));
        for _ in 0..4 {
            w.run(&["display-message", "-p", "x"]);
        }
        out.push(log_lines(&w.run(&["show-messages"])));
        out.push(w.run(&["set", "-g", "message-limit", "0"]));
        out.push(log_lines(&w.run(&["show-messages"])));
        out
    });
}

/// `show-messages -J` (job.c:436-450): one `Job N: cmd [fd, pid, status]`
/// line per job, newest first; fd and pid vary and are masked.
#[test]
fn show_messages_lists_jobs() {
    compare("show_messages_lists_jobs", |w| {
        let mask = |mut r: Run| {
            for key in ["fd=", "pid="] {
                while let Some(at) = r.stdout.find(key) {
                    let n = r.stdout[at + key.len()..]
                        .bytes()
                        .take_while(|b| b.is_ascii_digit() || *b == b'-')
                        .count();
                    r.stdout
                        .replace_range(at..at + key.len() + n, &key.replace('=', "#"));
                }
            }
            r
        };
        let mut out = vec![w.run(&["new", "-d", "-s", "a", "cat"])];
        out.push(w.run(&["show-messages", "-J"]));
        out.push(w.run(&["run-shell", "-b", "sleep 30"]));
        out.push(w.run(&["run-shell", "-b", "sleep 31"]));
        out.push(mask(w.run(&["show-messages", "-J"])));
        out.push(mask(w.run(&["show-messages", "-JT"])));
        out
    });
}

/// `clear-history` (cmd-capture-pane.c:424-431): history goes, the visible
/// screen stays, the pane leaves copy mode, `-H` is accepted, a new target
/// error is the usual one; output after the clear collects history again.
/// Reference: the same commands on the isolated reference server.
#[test]
fn clear_history_keeps_the_screen() {
    compare("clear_history_keeps_the_screen", |w| {
        let fmt = "#{history_size} #{pane_in_mode} #{cursor_y}";
        let mut out = vec![
            w.run(&["set", "-g", "history-limit", "500"]),
            w.run(&[
                "new",
                "-d",
                "-s",
                "a",
                "-x",
                "20",
                "-y",
                "5",
                "sh",
                "-c",
                "seq 1 40; exec cat",
            ]),
        ];
        w.wait_for_text("a", "40");
        out.push(w.run(&["display", "-p", "-t", "a", fmt]));
        out.push(w.run(&["capture-pane", "-p", "-t", "a"]));
        out.push(w.run(&["capture-pane", "-p", "-S", "-", "-E", "-1", "-t", "a"]));
        out.push(w.run(&["copy-mode", "-t", "a"]));
        out.push(w.run(&["display", "-p", "-t", "a", fmt]));
        out.push(w.run(&["clear-history", "-t", "a"]));
        out.push(w.run(&["display", "-p", "-t", "a", fmt]));
        out.push(w.run(&["capture-pane", "-p", "-t", "a"]));
        out.push(w.run(&["capture-pane", "-p", "-S", "-", "-E", "-1", "-t", "a"]));
        out.push(w.run(&["clear-history", "-t", "a"]));
        out.push(w.run(&["clearhist", "-H", "-t", "a"]));
        out.push(w.run(&["display", "-p", "-t", "a", fmt]));
        out.push(w.run(&["send-keys", "-t", "a", "-l", "x\nx\nx\nx\nx\nx\nx\n"]));
        w.wait_for_text("a", "x");
        std::thread::sleep(Duration::from_millis(300));
        out.push(w.run(&["display", "-p", "-t", "a", fmt]));
        out.push(w.run(&["clear-history", "-t", "nosuch"]));
        out.push(w.run(&["clear-history", "extra"]));
        out
    });
}

/// A control client killed outright (SIGKILL, so no exit message) is lost
/// like any other (server-client.c:478-564, `CLIENT_DEAD`): the server and
/// its sessions survive. vtmux once recursed through its own client-closed
/// notification to the dead socket until the stack overflowed.
#[test]
fn killed_control_client_leaves_the_server_up() {
    compare("killed_control_client_leaves_the_server_up", |w| {
        let mut out = vec![w.run(&["new", "-d", "-s", "s", "cat"])];
        w.run(&["set-hook", "-g", "client-closed", "set -g @closed 1"]);
        // stdin held open: a control client exits on stdin EOF.
        let mut c = Command::new(&w.bin)
            .args(["-L", &w.label, "-f", "/dev/null", "-C", "attach"])
            .current_dir(w.dir())
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env("TMUX_TMPDIR", w.dir())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn binary");
        let start = Instant::now();
        while w.run(&["lsc", "-F", "#{client_control_mode}"]).stdout != "1\n"
            && start.elapsed() < Duration::from_secs(5)
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        c.kill().expect("kill");
        let _ = c.wait();
        let start = Instant::now();
        while w.run(&["show", "-gv", "@closed"]).stdout != "1\n"
            && start.elapsed() < Duration::from_secs(5)
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        out.push(w.run(&["ls", "-F", "#{session_name}"]));
        out.push(w.run(&["lsc"]));
        out.push(w.run(&["show", "-gv", "@closed"]));
        out
    });
}

/// `choose-tree` with no client (cmd-choose-tree.c, window-tree.c):
/// `-O` must be a sort order, the target must be a pane; the mode is
/// entered once (a second `choose-tree` leaves it), `send-keys -X` is "not
/// in a mode" (no command callback), and a key `send-keys` gives with no
/// client does not reach it (window.c:2088), so `q` does not leave it.
#[test]
fn choose_tree_without_a_client() {
    compare("choose_tree_without_a_client", |w| {
        let mode = |w: &World| w.run(&["display", "-p", "#{pane_mode}|#{pane_in_mode}"]);
        let mut runs = vec![
            w.run(&["new", "-d", "-s", "a", "cat"]),
            w.run(&["choose-tree", "-O", "bogus"]),
            w.run(&["choose-tree", "-t", "nosuch"]),
            w.run(&["choose-tree", "-N", "-O", "SIZE"]),
        ];
        runs.push(mode(w));
        runs.push(w.run(&["choose-tree", "-N"]));
        runs.push(mode(w));
        runs.push(w.run(&["send-keys", "-X", "cancel"]));
        runs.push(w.run(&["send-keys", "-N", "2"]));
        runs.push(w.run(&["send-keys", "q"]));
        runs.push(mode(w));
        runs.push(w.run(&["copy-mode"]));
        runs.push(mode(w));
        runs.push(w.run(&["send-keys", "-X", "cancel"]));
        runs.push(mode(w));
        runs
    });
}

/// How many processes have `marker` in their command line, once it is `want`
/// or 5 s have passed.
fn procs_with(marker: &str, want: usize) -> usize {
    let start = Instant::now();
    loop {
        let out = Command::new("pgrep")
            .args(["-f", "-c", marker])
            .output()
            .expect("pgrep");
        let n = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(0);
        if n == want || start.elapsed() > Duration::from_secs(5) {
            return n;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `#()` in formats (format.c `format_job_get`): each client has its own job
/// cache, so the output shows only to a later expansion in the *same*
/// client, hence the chained commands; jobs run in the client's directory
/// with the global environment, only the last line counts, the command and
/// the output are both expanded, `T:` runs strftime on the command first,
/// and a job that is still running after a second shows `<'cmd' not ready>`.
#[test]
fn format_jobs_chained() {
    compare("format_jobs_chained", |w| {
        let again = |fmt: &str| -> Run {
            let mut a = vec!["display", "-p", fmt, ";", "run", "-d1", "", ";"];
            a.extend(["display", "-p", fmt]);
            w.run(&a)
        };
        let mut runs = vec![
            w.run(&["new", "-d", "-s", "s", "cat"]),
            w.run(&["setenv", "-g", "GV", "gv"]),
            w.run(&["setenv", "SV", "sv"]),
            w.run(&["set", "-g", "@k", "#(echo %H)"]),
            // A fresh client never sees job output.
            w.run(&["display", "-p", "a[#(echo hi)]"]),
            w.run(&["display", "-p", "a[#(echo hi)]"]),
            again("b[#(echo hi)]"),
            again("#{?#(echo 1),a,b}"),
            again("#(echo P=$PWD G=$GV S=$SV)"),
            again("#(printf 'a\\nb\\nc')"),
            again("#(printf 'a\\r\\nb')"),
            again("#(echo \"##{session_name}\")"),
            again("#(echo %%H)"),
            again("#(echo '#(echo nested)')"),
            again("#{S:#(echo s)}#{P:#(echo p)}#{W:#(echo w)}"),
            again("#(echo #{session_name})"),
            again("#(exit 3)"),
            again("#(echo err >&2)"),
            again("#(true)"),
        ];
        // T: runs strftime on the command: no `%H` is left over.
        let t = again("#{T:@k}");
        runs.push(Run {
            stdout: t
                .stdout
                .chars()
                .map(|c| if c.is_ascii_digit() { 'N' } else { c })
                .collect(),
            ..t
        });
        // Still running after a second: the placeholder.
        runs.push(w.run(&[
            "display",
            "-p",
            "x<#(exec sleep 5)>",
            ";",
            "run",
            "-d3",
            "",
            ";",
            "display",
            "-p",
            "x<#(exec sleep 5)>",
        ]));
        runs
    });
}

/// A client's jobs go with it (`format_lost_client`, format.c:515-518), and
/// the server's with the server (`job_kill_all`); only the job's own process
/// is signalled.
#[test]
fn format_jobs_do_not_leak() {
    let marker = format!("sleep 600.{}", std::process::id());
    compare("format_jobs_do_not_leak", |w| {
        let cmd = format!("#(exec {marker})");
        let mut runs = vec![w.run(&["new", "-d", "-s", "s", "cat"])];
        let count = |want: usize| Run {
            stdout: format!("{}\n", procs_with(&marker, want)),
            stderr: String::new(),
            code: 0,
        };
        // A client's job lives while the client does. `[.]` keeps the
        // counting shell's own command line out of the match.
        let alive = format!("pgrep -fc 'sleep 600[.]{}'", std::process::id());
        runs.push(w.run(&[
            "display", "-p", &cmd, ";", "run", "-d1", "", ";", "run", &alive,
        ]));
        runs.push(count(0));
        // One with no client (a window name's) goes with the server.
        runs.push(w.run(&["set", "-g", "automatic-rename-format", &cmd]));
        runs.push(w.run(&["new-window", "-d", "-n", "x", "cat"]));
        runs.push(count(1));
        runs.push(w.run(&["kill-server"]));
        runs.push(count(0));
        runs
    });
}

/// `#()` through `set -F`, `if -F` and a loop (the status-less users of
/// `format_job_get`), chained like [`format_jobs_chained`].
#[test]
fn format_jobs_in_commands() {
    compare("format_jobs_in_commands", |w| {
        vec![
            w.run(&["new", "-d", "-s", "s", "cat"]),
            w.run(&[
                "set",
                "-gF",
                "@f",
                "#(echo hi)",
                ";",
                "run",
                "-d1",
                "",
                ";",
                "set",
                "-gF",
                "@f",
                "#(echo hi)",
                ";",
                "show",
                "-gv",
                "@f",
            ]),
            w.run(&[
                "if",
                "-F",
                "#(echo 1)",
                "display -p y",
                "display -p n",
                ";",
                "run",
                "-d1",
                "",
                ";",
                "if",
                "-F",
                "#(echo 1)",
                "display -p y",
                "display -p n",
            ]),
            w.run(&[
                "list-clients",
                "-F",
                "#(echo hi)",
                ";",
                "run",
                "-d1",
                "",
                ";",
                "display",
                "-p",
                "-t",
                "s",
                "#{L:#(echo l)}",
            ]),
        ]
    });
}

/// `#{pane_key_mode}` (format.c `format_cb_pane_key_mode`) after the
/// program's modifyOtherKeys requests (input.c:1527-1559), under each
/// `extended-keys`: `off` ignores them, `always` keeps mode 1 at least. A
/// pane made under `always` starts in mode 1 (`screen_reinit`), as does a
/// respawned one; RIS goes back to the reset mode (`screen_write_reset`);
/// a pane in a mode shows the mode screen's key mode, fixed when the mode
/// began (`screen_init`).
#[test]
fn pane_key_mode_follows_requests_and_extended_keys() {
    const KEY_MODE: &str = "#{pane_key_mode}";
    compare("pane_key_mode_follows_requests_and_extended_keys", |w| {
        let key_mode = |w: &World, target: &str| w.run(&["display", "-p", "-t", target, KEY_MODE]);
        // `printf BYTES` in a fresh pane, then the mode once it printed.
        // A fresh server each time, once the last one has gone.
        let restart = |w: &World| {
            w.run(&["kill-server"]);
            w.wait_gone();
        };
        let after = |w: &World, ek: &str, bytes: &str| {
            restart(w);
            let script = format!("printf '{bytes}'; echo DONE; exec cat");
            w.run(&[
                "set",
                "-g",
                "extended-keys",
                ek,
                ";",
                "new",
                "-d",
                "-s",
                "a",
                "sh",
                "-c",
                &script,
            ]);
            w.wait_for_text("a", "DONE\n");
            let mut r = key_mode(w, "a");
            r.stdout = format!("{ek} {bytes}: {}", r.stdout);
            r
        };
        let mut runs = Vec::new();
        for ek in ["off", "on", "always"] {
            for bytes in [
                "",
                r"\033[>4;1m",
                r"\033[>4;2m",
                r"\033[>4m",
                r"\033[>4;2m\033[>4n",
                r"\033[>4;0m",
                r"\033[>4;2m\033[>5n",
                r"\033[>4;2m\033c",
                r"\033[>4;3m",
                r"\033[>4;2m\033[>4;1m",
                r"\033[>4;2m\033[>m",
                r"\033[>4;2m\033[?4m",
                r"\033[>4;2;5m",
                r"\033[>4:2m",
                r"\033[>u\033[>1u",
            ] {
                runs.push(after(w, ek, bytes));
            }
        }
        // Panes made before and after `extended-keys always`, a respawn.
        restart(w);
        runs.push(w.run(&["new", "-d", "-s", "a", "cat"]));
        runs.push(w.run(&["set", "-g", "extended-keys", "always"]));
        runs.push(w.run(&["neww", "-d", "-t", "a:1", "cat"]));
        runs.push(key_mode(w, "a:0"));
        runs.push(key_mode(w, "a:1"));
        runs.push(w.run(&["respawn-pane", "-k", "-t", "a:0", "cat"]));
        runs.push(key_mode(w, "a:0"));
        // Mode screens: copy mode entered under `on` and `always`, clock
        // mode, choose-tree.
        runs.push(w.run(&["set", "-g", "extended-keys", "on"]));
        runs.push(w.run(&[
            "neww",
            "-d",
            "-t",
            "a:2",
            "sh",
            "-c",
            r"printf '\033[>4;2m'; echo DONE; exec cat",
        ]));
        w.wait_for_text("a:2", "DONE\n");
        runs.push(key_mode(w, "a:2"));
        runs.push(w.run(&["copy-mode", "-t", "a:2"]));
        runs.push(key_mode(w, "a:2"));
        runs.push(w.run(&["set", "-g", "extended-keys", "always"]));
        runs.push(key_mode(w, "a:2"));
        runs.push(w.run(&["send-keys", "-t", "a:2", "-X", "cancel"]));
        runs.push(key_mode(w, "a:2"));
        runs.push(w.run(&["copy-mode", "-t", "a:2"]));
        runs.push(w.run(&["set", "-g", "extended-keys", "on"]));
        runs.push(key_mode(w, "a:2"));
        runs.push(w.run(&["send-keys", "-t", "a:2", "-X", "cancel"]));
        runs.push(w.run(&["clock-mode", "-t", "a:2"]));
        runs.push(key_mode(w, "a:2"));
        runs.push(w.run(&["send-keys", "-t", "a:2", "q"]));
        runs.push(w.run(&["set", "-g", "extended-keys", "always"]));
        runs.push(w.run(&["choose-tree", "-t", "a:2"]));
        runs.push(key_mode(w, "a:2"));
        runs
    });
}

/// What `send-keys` writes for modified keys after the program asked for
/// modifyOtherKeys 1 or 2, in both `extended-keys-format`s
/// (input-keys.c:424-473, 544-568, 674-707); the owner's config sets
/// `extended-keys on` and `extended-keys-format csi-u`. The pane copies
/// its raw input to a file; `|` (sent with -H) separates the keys.
#[test]
fn send_keys_bytes_follow_the_pane_key_mode() {
    const KEYS: [&str; 18] = [
        "C-a", "S-a", "C-S-a", "M-a", "S-Enter", "C-Enter", "C-Tab", "BTab", "S-Tab", "C-1", "C-é",
        "C-BSpace", "C-Escape", "C-i", "C-[", "C-M-S-x", "S-Up", "M-BTab",
    ];
    compare("send_keys_bytes_follow_the_pane_key_mode", |w| {
        let mut runs = Vec::new();
        for mode in ["", r"\033[>4;1m", r"\033[>4;2m"] {
            for format in ["csi-u", "xterm"] {
                w.run(&["kill-server"]);
                w.wait_gone();
                let out = w.dir().join("out");
                let script = format!(
                    "stty raw -echo; printf '{mode}'; exec cat > {}",
                    out.display()
                );
                runs.push(w.run(&[
                    "set",
                    "-g",
                    "extended-keys",
                    "on",
                    ";",
                    "set",
                    "-g",
                    "extended-keys-format",
                    format,
                    ";",
                    "new",
                    "-d",
                    "-s",
                    "a",
                    "sh",
                    "-c",
                    &script,
                ]));
                // The mode request has been read once the mode shows.
                let want = match mode {
                    "" => "VT10x",
                    r"\033[>4;1m" => "Ext 1",
                    _ => "Ext 2",
                };
                let start = Instant::now();
                while w
                    .run(&["display", "-p", "-t", "a", "#{pane_key_mode}"])
                    .stdout
                    .trim()
                    != want
                {
                    assert!(start.elapsed() < Duration::from_secs(5), "{mode}");
                    std::thread::sleep(Duration::from_millis(20));
                }
                for key in KEYS {
                    w.run(&["send-keys", "-t", "a", key]);
                    w.run(&["send-keys", "-t", "a", "-H", "7c"]);
                }
                w.run(&["send-keys", "-t", "a", "-H", "2e"]);
                let start = Instant::now();
                loop {
                    std::thread::sleep(Duration::from_millis(20));
                    let bytes = std::fs::read(&out).unwrap_or_default();
                    if bytes.ends_with(b".") {
                        let keys: Vec<String> = bytes[..bytes.len() - 1]
                            .split(|b| *b == b'|')
                            .map(|k| String::from_utf8_lossy(k).escape_debug().to_string())
                            .collect();
                        runs.push(Run::seen(format!("{mode} {format}: {}", keys.join(" "))));
                        break;
                    }
                    assert!(start.elapsed() < Duration::from_secs(5), "{mode} {format}");
                }
            }
        }
        runs
    });
}
