//! tmux-resurrect and tmux-continuum, the plugins that bring sessions back
//! after a reboot, run against vtmux and the reference tmux 3.8 side by side
//! (the copies in `compat/tmux-plugins`, MIT). Each side builds the same
//! sessions, saves them with the plugin, kills its server, starts a new one
//! and restores: what comes back (sessions, windows, layouts, names, working
//! directories, pane titles, pane contents, restarted programs) and what was
//! saved must be the same on both sides.
//!
//! Every side has its own `/tmp` root: `HOME`, `TMUX_TMPDIR`,
//! `XDG_STATE_HOME`, the plugin's save directory and a `bin/` first on `PATH`
//! with `tmux` (the binary under test, as the plugins call it), `ps` and
//! `systemctl` (see [`Side::new`]).
mod common;
mod par;

use std::fmt::Write as _;
use std::io::Read as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// The vendored plugins, or `None` (the test is skipped, and says so) when
/// they are missing.
fn plugins() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../compat/tmux-plugins");
    let found = dir.join("tmux-resurrect/scripts/save.sh").exists()
        && dir.join("tmux-continuum/continuum.tmux").exists();
    if !found {
        eprintln!(
            "SKIP: tmux-resurrect or tmux-continuum missing in {}",
            dir.display()
        );
        return None;
    }
    Some(dir.canonicalize().unwrap())
}

fn bins() -> Vec<PathBuf> {
    let reference = std::env::var_os("VTMUX_REFERENCE_TMUX").map_or_else(
        || PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache/vtmux/tmux-3.8/bin/tmux"),
        PathBuf::from,
    );
    assert!(
        reference.exists(),
        "reference tmux missing: {}",
        reference.display()
    );
    vec![PathBuf::from(env!("CARGO_BIN_EXE_vtmux")), reference]
}

/// Whether vim is installed where the panes look (`/usr/bin:/bin`); the
/// test restarts it when it is, on both sides alike.
fn have_vim() -> bool {
    ["/usr/bin/vim", "/bin/vim"]
        .iter()
        .any(|p| Path::new(p).exists())
}

/// What a pane that never settles shows of itself besides its text.
const DIAGNOSIS: &str = "alternate #{alternate_on}, #{pane_width}x#{pane_height}, \
    scroll region #{scroll_region_upper}-#{scroll_region_lower}, \
    cursor #{cursor_x},#{cursor_y}, history #{history_size}";

/// The longest any state another process brings about is waited for: only
/// ends a hang (a restore runs a hundred `tmux` commands, slow under load).
const LIMIT: Duration = Duration::from_secs(60);

struct Side {
    bin: PathBuf,
    label: String,
    /// Owns the root (removed on drop, after the server is gone).
    _dir: tempfile::TempDir,
    /// The root, canonical: panes report their paths resolved (macOS's
    /// `/tmp` is `/private/tmp`).
    root: PathBuf,
    plugins: PathBuf,
}

impl Side {
    fn new(bin: &Path, plugins: &Path) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let label = format!(
            "res{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        // Under /tmp, not $TMPDIR: the socket path must fit `sun_path`.
        let dir = tempfile::Builder::new()
            .prefix("vtr")
            .tempdir_in("/tmp")
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        for d in [
            "bin", "sh", "home", "tt", "st", "res", "w/a", "w/b", "w/c d",
        ] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::os::unix::fs::symlink(bin, root.join("bin/tmux")).unwrap();
        // continuum saves and restores only when it sees no other tmux
        // server of the user's (`ps -u <uid> -o 'command pid'`, lines starting
        // `tmux`): a shared host or a developer's own tmux would turn it off.
        // It is shown this side's processes alone.
        let ps = format!(
            "#!/bin/sh\ncase \" $* \" in\n*\" -u \"*) /bin/ps \"$@\" | grep -e '^COMMAND' -e ' -L {label} ' ;;\n*) exec /bin/ps \"$@\" ;;\nesac\n"
        );
        // continuum runs `systemctl --user disable tmux.service` when booting
        // tmux is off (the default) on a systemd host: never the user's.
        // The panes' shell: a login shell reads the host's /etc/profile, which
        // can print (and wait); this one is `sh` without it.
        let scripts = [
            ("bin/ps", ps.as_str()),
            ("bin/systemctl", "#!/bin/sh\nexit 0\n"),
            ("sh/sh", "#!/bin/sh\nexec /bin/sh \"$@\"\n"),
        ];
        for (name, text) in scripts {
            let path = root.join(name);
            std::fs::write(&path, text).unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        // The prompt of every interactive sh, the restored ones too: they
        // are `exec $SHELL` after a `sh -c`, which drops PS1, and read $ENV.
        std::fs::write(root.join("shrc"), "PS1='$ '\n").unwrap();
        let mut text = String::new();
        for i in 1..=200 {
            let _ = writeln!(text, "line {i}");
        }
        std::fs::write(root.join("w/a/file.txt"), text).unwrap();
        Self {
            bin: bin.to_owned(),
            label,
            _dir: dir,
            root,
            plugins: plugins.to_owned(),
        }
    }

    fn env(&self) -> Vec<(&'static str, String)> {
        let path = |p: &str| self.root.join(p).display().to_string();
        vec![
            ("HOME", path("home")),
            ("TMUX_TMPDIR", path("tt")),
            ("XDG_STATE_HOME", path("st")),
            ("PATH", format!("{}:/usr/bin:/bin", path("bin"))),
            ("SHELL", path("sh/sh")),
            ("ENV", path("shrc")),
            ("TERM", "xterm-256color".into()),
            ("LC_ALL", "C.UTF-8".into()),
        ]
    }

    /// The config the next server starts with.
    fn config(&self, extra: &str) {
        let res = self.root.join("res");
        let p = self.plugins.display();
        let text = format!(
            "set -g status-right R\n\
             set -g base-index 1\n\
             set -g @resurrect-capture-pane-contents on\n\
             set -g @resurrect-dir '{}'\n\
             {extra}\
             run-shell '{p}/tmux-resurrect/resurrect.tmux'\n",
            res.display()
        );
        std::fs::write(self.root.join("tmux.conf"), text).unwrap();
    }

    /// `tmux -L <label> -f <config> args`, run as `tmux` (the plugins check
    /// `tmux -V`, continuum looks for `tmux` in `ps`).
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(self.root.join("bin/tmux"));
        cmd.arg0("tmux")
            .arg("-L")
            .arg(&self.label)
            .arg("-f")
            .arg(self.root.join("tmux.conf"))
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .envs(self.env());
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        common::output_within(&mut self.command(args), LIMIT)
            .unwrap_or_else(|| panic!("{}: {args:?} hung", self.bin.display()))
    }

    /// `run`, which must succeed: its stdout.
    fn t(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{}: {args:?}: {}",
            self.bin.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn dir(&self, d: &str) -> String {
        self.root.join("w").join(d).display().to_string()
    }

    /// The pane's visible rows, trailing blanks trimmed, blank rows at the
    /// bottom dropped.
    fn screen(&self, target: &str) -> Vec<String> {
        lines(&self.t(&["capture-pane", "-p", "-t", target]))
    }

    fn command_in(&self, target: &str) -> String {
        self.t(&["display", "-p", "-t", target, "#{pane_current_command}"])
            .trim_end()
            .to_owned()
    }

    fn wait(&self, what: &str, mut done: impl FnMut() -> bool) {
        let end = Instant::now() + LIMIT;
        while !done() {
            assert!(
                Instant::now() < end,
                "{}: timed out waiting for {what}",
                self.bin.display()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait until `target` runs `program` and its screen passes `check`.
    fn wait_pane(&self, target: &str, program: &str, check: impl Fn(&[String]) -> bool) {
        let end = Instant::now() + LIMIT;
        loop {
            let running = self.command_in(target);
            let screen = self.screen(target);
            if running == program && check(&screen) {
                return;
            }
            assert!(
                Instant::now() < end,
                "{}: timed out waiting for {target} to run {program}: {running} {screen:#?}\n{}",
                self.bin.display(),
                self.t(&["capture-pane", "-p", "-S", "-", "-t", target])
                    + &self.t(&["display", "-p", "-t", target, DIAGNOSIS])
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Bring a restarted `program` (less or vim showing `file.txt`) in
    /// `target` to a screen that depends on nothing but its size.
    /// resurrect's restore types the command right after laying the window
    /// out, so the program may draw for a size its pane had a moment
    /// before and miss the change (SIGWINCH before its handler is in), under
    /// tmux as here, or get one more redraw on one side than on the other.
    /// So it is sent SIGWINCH, to draw at the size it has, and `key` (a
    /// redraw) clears the line its first screen alone shows (less's file
    /// name, vim's file message). Then it shows the first rows of the file,
    /// less's prompt `:` on the last row, vim's an empty one.
    fn redraw(&self, target: &str, program: &str, key: &str) {
        self.wait_pane(target, program, |x| has(x, "line"));
        // Every process under the pane's: the program is the shell's child
        // where `/bin/sh` execs the shell, a grandchild where it does not.
        let mut parents = vec![self.t(&["display", "-p", "-t", target, "#{pane_pid}"])];
        while let Some(parent) = parents.pop() {
            let children = Command::new("pgrep")
                .args(["-P", parent.trim()])
                .output()
                .unwrap();
            for pid in String::from_utf8_lossy(&children.stdout).split_whitespace() {
                let raw = rustix::process::Pid::from_raw(pid.parse().unwrap()).unwrap();
                let _ = rustix::process::kill_process(raw, rustix::process::Signal::WINCH);
                parents.push(pid.to_owned());
            }
        }
        self.t(&["send-keys", "-t", target, key]);
        let h: usize = self
            .t(&["display", "-p", "-t", target, "#{pane_height}"])
            .trim()
            .parse()
            .unwrap();
        let last_text = format!("line {}", h - 1);
        self.wait_pane(target, program, |x| {
            let rows = if program == "less" { h } else { h - 1 };
            x.len() == rows
                && x[0] == "line 1"
                && x[h - 2] == last_text
                && (program != "less" || x[h - 1] == ":")
        });
    }

    /// Wait until `window`, named automatically, is named after the
    /// command its pane runs ([`Side::wait_shell`] first: its shell). tmux
    /// names a new window after its pane's process before the shell has
    /// replaced it (`tmux` for a while), and renames it on a timer:
    /// resurrect saves and restores whatever name the window has then.
    fn wait_named_after_its_command(&self, window: &str) {
        self.wait(&format!("{window} to be named after its command"), || {
            let name = self.t(&["display", "-p", "-t", window, "#{window_name}"]);
            name.trim_end() == self.command_in(window)
        });
    }

    /// Wait until the shell in `target` shows its prompt and its screen
    /// passes `check` (which counts the prompts: a restored pane's saved
    /// screen ends in one too). The command is not looked at: what
    /// `/bin/sh` runs as is the host's (`sh`, `dash`, `bash` on macOS), and
    /// a bare prompt on the last row is the shell's.
    fn wait_shell(&self, target: &str, check: impl Fn(&[String]) -> bool) {
        let end = Instant::now() + LIMIT;
        loop {
            let screen = self.screen(target);
            if prompts(&screen) > 0 && check(&screen) {
                return;
            }
            assert!(
                Instant::now() < end,
                "{}: timed out waiting for the shell in {target}: {screen:#?}\n{}",
                self.bin.display(),
                self.t(&["capture-pane", "-p", "-S", "-", "-t", target])
                    + &self.t(&["display", "-p", "-t", target, DIAGNOSIS])
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait for the shell's prompt in `target`, then type `line` and Enter.
    fn shell(&self, target: &str, line: &str) {
        self.wait_shell(target, |s| prompts(s) == 1);
        self.t(&["send-keys", "-t", target, line, "Enter"]);
    }

    fn kill_server(&self) {
        self.t(&["kill-server"]);
        common::wait_gone(|| {
            let ls = self.run(&["ls"]);
            (
                ls.status.success(),
                String::from_utf8_lossy(&ls.stderr).into_owned(),
            )
        });
    }

    /// What resurrect wrote: its layout file and the saved pane contents.
    fn saved(&self) -> String {
        let res = self.root.join("res");
        let mut out = std::fs::read_to_string(res.join("last")).unwrap();
        let pc = self.root.join("pc");
        let _ = std::fs::remove_dir_all(&pc);
        std::fs::create_dir(&pc).unwrap();
        let tar = Command::new("tar")
            .arg("xzf")
            .arg(res.join("pane_contents.tar.gz"))
            .arg("-C")
            .arg(&pc)
            .status()
            .unwrap();
        assert!(tar.success());
        let mut files: Vec<_> = std::fs::read_dir(pc.join("pane_contents"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        files.sort();
        for f in files {
            let name = f.file_name().unwrap().to_string_lossy().into_owned();
            let _ = writeln!(out, "== {name}\n{}", std::fs::read_to_string(&f).unwrap());
        }
        self.normalize(&out)
    }

    /// Sessions, windows, panes and each pane's screen. A restarted `tail`
    /// shows only its own output: where its typed command line lands in the
    /// restored contents depends on when the shell read it.
    fn dump(&self) -> String {
        let mut out = String::new();
        out += &self.t(&[
            "ls",
            "-F",
            "#{session_name} #{session_windows} #{session_grouped}",
        ]);
        out += &self.t(&[
            "lsw",
            "-a",
            "-F",
            "#{session_name}:#{window_index} #{window_name} #{window_active} #{window_flags} #{window_layout} #{automatic-rename}",
        ]);
        let panes = self.t(&[
            "lsp",
            "-a",
            "-F",
            "#{session_name}:#{window_index}.#{pane_index} #{pane_active} #{pane_width}x#{pane_height} #{pane_title} #{pane_current_command} #{pane_current_path}",
        ]);
        out += &panes;
        for line in panes.lines() {
            let target = line.split(' ').next().unwrap();
            let mut screen = self.screen(target);
            if self.command_in(target) == "tail" {
                screen.drain(..screen.len().saturating_sub(10));
            }
            let _ = writeln!(out, "== {target}\n{}", screen.join("\n"));
        }
        self.normalize(&out)
    }

    fn normalize(&self, s: &str) -> String {
        s.replace(&self.root.display().to_string(), "<R>")
    }
}

impl Drop for Side {
    fn drop(&mut self) {
        common::teardown(&self.root.join("tt"), &self.label, None, |args| {
            self.command(args)
        });
    }
}

fn lines(s: &str) -> Vec<String> {
    let mut v: Vec<String> = s.lines().map(|l| l.trim_end().to_owned()).collect();
    while v.last().is_some_and(String::is_empty) {
        v.pop();
    }
    v
}

/// How many rows are a bare prompt, the last row being one: a restored
/// pane's shell has started once its prompt follows the saved one.
fn prompts(screen: &[String]) -> usize {
    if screen.last().map(String::as_str) != Some("$") {
        return 0;
    }
    screen.iter().filter(|l| *l == "$").count()
}

fn has(screen: &[String], text: &str) -> bool {
    screen.iter().any(|l| l.contains(text))
}

/// The panes of [`build`] and how each looks when settled: `restored` after
/// a restore, when every shell's saved screen has a new prompt under it.
fn settled(s: &Side, restored: bool) {
    let n = 1 + usize::from(restored);
    s.wait_shell("work:1.0", |x| prompts(x) == 1);
    // Before the save each program started in a pane made for it, at its
    // size: its first screen is saved as it is (vim's has cells written
    // and erased, which `capture-pane -J` keeps). After the restore it is
    // brought to a screen that only depends on its size (`Side::redraw`).
    let top = |x: &[String]| x.first().is_some_and(|l| l == "line 1");
    if restored {
        s.redraw("work:1.1", "less", "r");
    } else {
        s.wait_pane("work:1.1", "less", |x| top(x) && has(x, "file.txt"));
    }
    s.wait_shell("work:2.0", |x| prompts(x) == n && has(x, "hello-logs-0"));
    s.wait_shell("work:2.1", |x| prompts(x) == 1);
    s.wait_shell("work:2.2", |x| prompts(x) == n && has(x, "60"));
    s.wait_shell("misc:1.0", |x| prompts(x) == n && has(x, "misc-pane"));
    s.wait_named_after_its_command("misc:1");
    if have_vim() && restored {
        s.redraw("misc:2.1", "vim", "C-l");
    } else if have_vim() {
        // Not vim's '"file.txt" 200L' message: a vim with a different
        // shortmess or build (CI's Ubuntu) doesn't show it; the file's
        // lines from the top are what both servers must show.
        s.wait_pane("misc:2.1", "vim", top);
    }
    // Its saved screen ends in the file's last line too: a restarted tail
    // has printed it once more.
    s.wait_pane("misc:2.0", "tail", |x| {
        x.last().is_some_and(|l| l == "line 200")
    });
    if restored {
        s.wait("tail to start again", || {
            let all = s.t(&["capture-pane", "-p", "-S", "-", "-t", "misc:2.0"]);
            all.matches("line 200").count() >= 2
        });
    }
}

/// Two sessions and a grouped one: splits both ways, working directories
/// (one with a space, one changed with `cd`), shells with output, less, vim
/// and tail running, a zoomed pane, pane titles, renamed windows.
fn build(s: &Side) {
    s.t(&["new", "-d", "-s", "work", "-n", "edit", "-c", &s.dir("a")]);
    s.t(&["splitw", "-h", "-t", "work:edit", "-c", &s.dir("b")]);
    s.shell("work:1.1", "cd ../a && less file.txt");
    s.t(&[
        "neww",
        "-d",
        "-t",
        "work:",
        "-n",
        "logs",
        "-c",
        &s.dir("c d"),
    ]);
    s.t(&["splitw", "-v", "-t", "work:logs", "-c", &s.dir("a")]);
    s.t(&["splitw", "-h", "-t", "work:logs.1", "-c", &s.dir("b")]);
    s.shell("work:2.0", "echo hello-logs-0");
    s.shell("work:2.2", "seq 1 60");
    s.t(&["select-pane", "-t", "work:2.1"]);
    s.t(&["resize-pane", "-Z", "-t", "work:2"]);
    s.t(&["new", "-d", "-s", "misc", "-c", &s.dir("b")]);
    s.shell("misc:1.0", "echo misc-pane; cd ..");
    // vim goes in the bottom pane: resurrect's restore splits a window
    // and makes the new pane as tall as it can (`resize-pane -U 999`)
    // before laying it out, so the pane above is a row high for a moment.
    // A program started then can read that size from its tty: tmux
    // resizes a pane's tty on a timer and, when queued sizes end where
    // they began, sends the first of them before the last (server-client.c
    // `server_client_check_pane_resize`). vim then sets a scroll region of
    // two rows, which it never sets again when its pane grows back. The
    // bottom pane is never smaller than at the end.
    s.t(&["neww", "-d", "-t", "misc:", "-n", "zed", "-c", &s.dir("b")]);
    s.t(&["splitw", "-t", "misc:zed", "-c", &s.dir("a")]);
    s.shell("misc:2.0", "tail -f ../a/file.txt");
    if have_vim() {
        s.shell("misc:2.1", "vim -u NONE -N -n -i NONE file.txt");
    }
    s.t(&["new", "-d", "-s", "grp", "-t", "work"]);
    s.t(&["select-window", "-t", "grp:2"]);
    // A new pane's title is the host's name: pin them.
    for id in s.t(&["lsp", "-a", "-F", "#{pane_id}"]).lines() {
        s.t(&["select-pane", "-t", id, "-T", &format!("t{id}")]);
    }
}

/// What a side shows: what was saved and what came back.
struct Seen {
    saved: String,
    restored: String,
    /// What an idle shell's pane runs, as `#{pane_current_command}` says:
    /// the host's `/bin/sh` (`bash` on macOS).
    shell: String,
}

fn assert_same(seen: &[Seen], bins: &[PathBuf]) {
    let [vt, tmux] = seen else {
        panic!("two sides")
    };
    let name = |i: usize| bins[i].display().to_string();
    assert_eq!(
        vt.saved.lines().collect::<Vec<_>>(),
        tmux.saved.lines().collect::<Vec<_>>(),
        "saved: {} (left) vs {} (right)",
        name(0),
        name(1)
    );
    assert_eq!(
        vt.restored.lines().collect::<Vec<_>>(),
        tmux.restored.lines().collect::<Vec<_>>(),
        "restored: {} (left) vs {} (right)",
        name(0),
        name(1)
    );
}

/// resurrect's save (`prefix C-s` runs it with `run-shell`), the server
/// killed, a new one started and resurrect's restore (`prefix C-r`), with
/// pane contents on: vtmux saves and restores what tmux 3.8 does.
#[test]
fn resurrect_saves_and_restores_as_with_tmux() {
    let Some(plugins) = plugins() else { return };
    let bins = bins();
    let seen = par::map(&bins, 2, |bin| {
        let s = Side::new(bin, &plugins);
        s.config("");
        build(&s);
        settled(&s, false);
        let p = s.plugins.display();
        s.t(&[
            "run-shell",
            &format!("{p}/tmux-resurrect/scripts/save.sh quiet"),
        ]);
        let saved = s.saved();
        s.kill_server();
        s.t(&["new", "-d"]);
        s.t(&[
            "run-shell",
            &format!("{p}/tmux-resurrect/scripts/restore.sh"),
        ]);
        s.wait("the restored sessions", || {
            s.t(&["ls", "-F", "#{session_name}"]) == "grp\nmisc\nwork\n"
        });
        settled(&s, true);
        Seen {
            saved,
            restored: s.dump(),
            shell: s.command_in("work:1.0"),
        }
    });
    // What a user sees come back, on vtmux; the comparison does the rest.
    let (vt, sh) = (&seen[0].restored, &seen[0].shell);
    for want in [
        "work:1 edit 1 *",
        "work:2 logs 0 Z",
        "misc:2 zed 0",
        "work:1.1 1 39x24 t%1 less <R>/w/a\n",
        &format!("work:2.0 0 80x12 t%2 {sh} <R>/w/c d\n"),
        &format!("misc:1.0 1 80x24 t%5 {sh} <R>/w\n"),
        "misc:2.0 0 80x12 t%6 tail <R>/w/b\n",
        "== work:2.0\n$ echo hello-logs-0\nhello-logs-0\n$\n$\n",
        "== misc:1.0\n$ echo misc-pane; cd ..\nmisc-pane\n$\n$\n",
    ] {
        assert!(vt.contains(want), "{want:?} not in\n{vt}");
    }
    if have_vim() {
        assert!(vt.contains("misc:2.1 1 80x11 t%7 vim <R>/w/a\n"), "{vt}");
    }
    assert_same(&seen, &bins);
}

/// continuum saves from the status line of an attached client (its `#()`
/// job, every `@continuum-save-interval`) and, with `@continuum-restore on`,
/// restores as the next server starts, no command typed: as with tmux.
#[test]
fn continuum_saves_from_the_status_line_and_restores_at_start() {
    let Some(plugins) = plugins() else { return };
    let bins = bins();
    let seen = par::map(&bins, 2, |bin| {
        let s = Side::new(bin, &plugins);
        let p = s.plugins.display();
        // A last save long ago: the first status line saves. Plugins are
        // loaded with their output thrown away, as TPM does (continuum
        // traces itself with `set -x`).
        // continuum restores in the background: sessions show up long
        // before resurrect's restore has made each window's pane active
        // again. Its `post-restore-all` hook (run after all of that) says
        // when it is done.
        let continuum = format!(
            "set -g status-interval 1\n\
             set -g @continuum-save-last-timestamp 1\n\
             set -g @resurrect-hook-post-restore-all 'tmux set -g @restored 1'\n\
             run-shell '{p}/tmux-continuum/continuum.tmux >/dev/null 2>&1'\n"
        );
        s.config(&continuum);
        s.t(&["new", "-d", "-s", "work", "-n", "edit", "-c", &s.dir("a")]);
        s.t(&["splitw", "-h", "-t", "work:edit", "-c", &s.dir("b")]);
        s.shell("work:1.1", "echo pane-one");
        s.t(&["neww", "-d", "-t", "work:", "-n", "logs", "-c", &s.dir("b")]);
        s.t(&["new", "-d", "-s", "misc", "-c", &s.dir("b")]);
        for id in s.t(&["lsp", "-a", "-F", "#{pane_id}"]).lines() {
            s.t(&["select-pane", "-t", id, "-T", &format!("t{id}")]);
        }
        // The first status line saves: every pane settled before it.
        s.wait_shell("work:1.0", |x| prompts(x) == 1);
        s.wait_shell("work:1.1", |x| prompts(x) == 1 && has(x, "pane-one"));
        s.wait_shell("work:2.0", |x| prompts(x) == 1);
        s.wait_shell("misc:1.0", |x| prompts(x) == 1);
        s.wait_named_after_its_command("misc:1");
        let res = s.root.join("res");
        let attached = attach(&s, "work");
        // The save runs in the background: done when its pane contents are
        // archived and their directory emptied.
        s.wait("continuum's save", || {
            res.join("pane_contents.tar.gz").exists()
                && std::fs::read_dir(res.join("save/pane_contents"))
                    .is_ok_and(|mut d| d.next().is_none())
        });
        let saved = s.saved();
        drop(attached);
        s.kill_server();
        s.config(&format!("set -g @continuum-restore on\n{continuum}"));
        s.t(&["new", "-d"]);
        s.wait("continuum's restore", || {
            s.t(&["show", "-gqv", "@restored"]).trim_end() == "1"
        });
        assert_eq!(s.t(&["ls", "-F", "#{session_name}"]), "misc\nwork\n");
        s.wait_shell("work:1.0", |x| prompts(x) == 1);
        s.wait_shell("work:1.1", |x| prompts(x) == 2 && has(x, "pane-one"));
        s.wait_shell("work:2.0", |x| prompts(x) == 1);
        s.wait_shell("misc:1.0", |x| prompts(x) == 1);
        s.wait_named_after_its_command("misc:1");
        Seen {
            saved,
            restored: s.dump(),
            shell: s.command_in("work:1.0"),
        }
    });
    let (vt, sh) = (&seen[0].restored, &seen[0].shell);
    for want in [
        "work:1 edit 1 *",
        "work:2 logs 0",
        &format!("work:1.1 0 39x23 t%1 {sh} <R>/w/b\n"),
        "== work:1.1\n$ echo pane-one\npane-one\n$\n$\n",
    ] {
        assert!(vt.contains(want), "{want:?} not in\n{vt}");
    }
    assert_same(&seen, &bins);
}

/// `tmux attach -t session` under an 80x24 PTY, drained in the background
/// so the client never stalls; killed on drop.
struct Attached {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Drop for Attached {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn attach(s: &Side, session: &str) -> Attached {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(s.root.join("bin/tmux"));
    cmd.args(["-L", &s.label, "attach", "-t", session]);
    cmd.cwd(&s.root);
    cmd.env_clear();
    for (k, v) in s.env() {
        cmd.env(k, v);
    }
    let child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0; 8192];
        while reader.read(&mut buf).is_ok_and(|n| n > 0) {}
    });
    s.wait("the client to attach", || {
        !s.t(&["lsc", "-F", "#{client_name}"]).is_empty()
    });
    Attached {
        child,
        _master: pair.master,
    }
}
