//! The old image's half of a live upgrade, up to the `execve`, which a
//! recorder stands in for: preflight refusals, the wait for work in
//! flight, the dry run on the real snapshot, the answers, access control
//! and the `-b` default.

use std::cell::RefCell;
use std::io;
use std::os::fd::{AsFd as _, AsRawFd as _, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use vtmux_core::protocol::{ControlMode, ServerMsg};
use vtmux_snapshot as snap;

use super::super::snapshot::tests as st;
use super::super::{ClientId, Server};

/// One call of the exec the recorder stood in for.
struct Exec {
    binary: PathBuf,
    /// The snapshot file's bytes, read from where the exec got it.
    bytes: Vec<u8>,
    keep: Vec<RawFd>,
}

thread_local! {
    static EXECS: RefCell<Vec<Exec>> = const { RefCell::new(Vec::new()) };
}

/// An [`super::ExecFn`] that records the call and fails, as an `execve`
/// that returns would.
fn record_exec(binary: &Path, file: OwnedFd, keep: &[RawFd], _socket: Option<&Path>) -> io::Error {
    let bytes = crate::sys::upgrade::read_all(file.as_fd()).unwrap();
    // The real exec closes it when it fails.
    drop(file);
    EXECS.with(|e| {
        e.borrow_mut().push(Exec {
            binary: binary.to_path_buf(),
            bytes,
            keep: keep.to_vec(),
        });
    });
    io::Error::other("test exec")
}

fn execs() -> Vec<Exec> {
    EXECS.with(|e| std::mem::take(&mut *e.borrow_mut()))
}

/// An executable `sh` script `name` in `dir`.
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A dry run that passes, keeping its arguments in `dir/args`.
fn passing(dir: &Path) -> PathBuf {
    let args = dir.join("args");
    script(dir, "pass", &format!("echo \"$1\" > '{}'", args.display()))
}

/// A server in `dir` with session `a`, its exec recorded.
fn setup(tag: &str) -> (Server, PathBuf) {
    let dir = st::temp_dir(&format!("up-{tag}"));
    let mut server = st::server(&dir);
    server.upgrade.exec = record_exec;
    st::ok(
        &mut server,
        &dir,
        &["new-session", "-d", "-s", "a", "cat", "-"],
    );
    execs();
    (server, dir)
}

/// What a command client was sent.
#[derive(Debug, Default)]
struct Answer {
    out: String,
    err: String,
    exit: Option<i32>,
}

/// The loop's upgrade turn at `at`, again while a dry run runs (at most
/// 20 s).
fn turns(server: &mut Server, at: Instant) {
    let end = Instant::now() + std::time::Duration::from_secs(20);
    loop {
        server.upgrade_turn(at);
        if server.upgrade_dry_run_pid().is_none() {
            return;
        }
        assert!(Instant::now() < end, "the dry run did not end");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// Run `words` as a command client, then the loop's upgrade turns at `at`
/// and the queues, until the client is told to exit.
fn command(server: &mut Server, dir: &Path, words: &[&str], at: Instant) -> Answer {
    let (id, mut peer) = st::connect(server, st::hello(words, dir, None, ControlMode::None));
    let mut answer = Answer::default();
    for _ in 0..3 {
        turns(server, at);
        server.cmdq_process();
        for (c, code) in std::mem::take(&mut server.pending_exits) {
            server.finish_client(c, code);
        }
        for m in st::messages(server, id, &mut peer) {
            match m {
                ServerMsg::Stdout(b) => answer.out.push_str(&String::from_utf8_lossy(&b)),
                ServerMsg::Stderr(b) => answer.err.push_str(&String::from_utf8_lossy(&b)),
                ServerMsg::Exit { code, .. } => answer.exit = Some(code),
                _ => {}
            }
        }
        if answer.exit.is_some() {
            break;
        }
    }
    server.drop_client(id);
    answer
}

fn upgrade(server: &mut Server, dir: &Path, binary: &Path) -> Answer {
    let b = binary.to_str().unwrap();
    command(server, dir, &["vtmux-upgrade", "-b", b], Instant::now())
}

/// A raw connection asking with the frozen `UpgradeRequest`.
fn handshake(server: &mut Server, binary: &Path) -> (ClientId, mio::net::UnixStream) {
    let (ours, peer) = mio::net::UnixStream::pair().unwrap();
    let id = ClientId(server.next_client);
    server.next_client += 1;
    server
        .clients
        .insert(id, super::super::Client::new(ours, 0));
    server.on_message(
        id,
        vtmux_core::protocol::ClientMsg::UpgradeRequest {
            binary: binary.to_path_buf(),
        },
    );
    (id, peer)
}

fn results(
    server: &mut Server,
    id: ClientId,
    peer: &mut mio::net::UnixStream,
) -> Vec<(bool, String)> {
    st::messages(server, id, peer)
        .into_iter()
        .filter_map(|m| match m {
            ServerMsg::UpgradeResult { ok, message } => Some((ok, message)),
            _ => None,
        })
        .collect()
}

fn has_session(server: &Server, name: &str) -> bool {
    server.model.sessions.values().any(|s| s.name == name)
}

/// Step 1: a binary that isn't there, isn't a file or can't be run is
/// refused at once, before anything waits.
#[test]
fn a_binary_that_cannot_run_is_refused_at_once() {
    let (mut server, dir) = setup("binary");
    let missing = dir.join("missing");
    let plain = dir.join("plain");
    std::fs::write(&plain, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
    for (binary, want) in [
        (
            &missing,
            format!("{}: No such file or directory\n", missing.display()),
        ),
        (&plain, format!("{}: Permission denied\n", plain.display())),
        (&dir, format!("{}: not a regular file\n", dir.display())),
    ] {
        let a = upgrade(&mut server, &dir, binary);
        assert_eq!((a.err.as_str(), a.exit), (want.as_str(), Some(1)));
        assert!(server.upgrade.pending.is_none());
    }
    // `-b` is the requester's: a relative one is from its directory.
    script(&dir, "rel", "exit 5");
    let a = command(
        &mut server,
        &dir,
        &["vtmux-upgrade", "-b", "rel"],
        Instant::now(),
    );
    assert_eq!(
        (a.err.as_str(), a.exit),
        (
            format!(
                "{} --vtmux-check-snapshot failed: exit status: 5\n",
                dir.join("rel").display()
            )
            .as_str(),
            Some(1)
        )
    );
    assert!(execs().is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

/// Steps 3 and 4: the dry run gets the snapshot's descriptor
/// (`--vtmux-check-snapshot <fd>`), and the exec gets the same bytes with
/// every descriptor the snapshot names kept. An exec that returns leaves
/// the server as it was, the failure logged and told to the requester.
#[test]
fn the_dry_run_and_the_exec_get_the_snapshot() {
    let (mut server, dir) = setup("pass");
    st::ok(&mut server, &dir, &["split-window", "-t", "a", "cat", "-"]);
    let binary = passing(&dir);
    let a = upgrade(&mut server, &dir, &binary);
    let refused = format!("{}: test exec", binary.display());
    assert_eq!(
        (a.out.as_str(), a.err.as_str(), a.exit),
        ("", format!("{refused}\n").as_str(), Some(1))
    );
    let args = std::fs::read_to_string(dir.join("args")).unwrap();
    assert_eq!(args, "--vtmux-check-snapshot\n");
    let exec = execs().pop().expect("exec");
    assert_eq!(exec.binary, binary);
    let s = snap::decode(&exec.bytes).unwrap();
    assert_eq!(s.meta.purpose, snap::Purpose::Upgrade);
    assert_eq!(
        s.sessions
            .iter()
            .map(|x| x.name.as_str())
            .collect::<Vec<_>>(),
        ["a"]
    );
    let mut roles: Vec<snap::Role> = s
        .descriptors
        .iter()
        .filter(|d| !matches!(d.role, snap::Role::Client(_)))
        .map(|d| d.role.clone())
        .collect();
    roles.sort_by_key(|r| format!("{r:?}"));
    assert_eq!(
        roles,
        [
            snap::Role::Listener(()),
            snap::Role::PanePty(0),
            snap::Role::PanePty(1)
        ]
    );
    for d in &s.descriptors {
        assert!(exec.keep.contains(&d.fd), "{d:?} not kept");
    }
    assert!(has_session(&server, "a") && server.upgrade.pending.is_none());
    // Nothing is left half done: another upgrade runs.
    upgrade(&mut server, &dir, &binary);
    assert_eq!(execs().len(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

/// Decision 23, read from the stream the exec gets: a binary that reports
/// this build's patch stack (`--vtmux-patch-stack`) gets the panes'
/// history in Ghostty's form, also history the compressor has not reached
/// (it waits for 5 s of quiet); one that reports another stack, or answers
/// nothing (a binary from before the flag), gets it all as rows, and no
/// requirement it can't meet. The history is the pane's either way.
#[test]
fn history_goes_in_ghosttys_form_only_to_the_same_patch_stack() {
    use crate::snapshot_term::{PATCH_STACK, ghostty_pages_requirement, term_from_snapshot};
    use vtmux_vt::{CaptureOpts, LineRef, capture};
    let (mut server, dir) = setup("pin");
    st::ok(&mut server, &dir, &["set", "-g", "history-limit", "20000"]);
    st::ok(
        &mut server,
        &dir,
        &[
            "new-session",
            "-d",
            "-s",
            "h",
            "-x",
            "80",
            "-y",
            "24",
            "cat",
            "-",
        ],
    );
    // Session `h`'s pane, the newest.
    let pane = *server.model.panes.keys().max().unwrap();
    let term = &mut server.model.panes.get_mut(&pane).unwrap().term;
    for i in 0..8000 {
        let line = format!(
            "\x1b[3{}m{i:05}\x1b[m line {}\r\n",
            i % 8,
            "h".repeat(i % 70)
        );
        term.feed(line.as_bytes());
    }
    let all = CaptureOpts {
        escapes: true,
        start: LineRef::Start,
        end: LineRef::End,
        ..CaptureOpts::default()
    };
    let shown = capture(term, all);
    let page_rows = u32::try_from(vtmux_vt::page_rows(80)).unwrap();
    let other = script(
        &dir,
        "other",
        "[ \"$1\" = --vtmux-patch-stack ] && echo 0123abcd; exit 0",
    );
    let same = script(
        &dir,
        "same",
        &format!("[ \"$1\" = --vtmux-patch-stack ] && echo {PATCH_STACK}; exit 0"),
    );
    // Rows first, while nothing is compressed yet; then the same stack.
    for (binary, ghostty) in [(&other, false), (&passing(&dir), false), (&same, true)] {
        upgrade(&mut server, &dir, binary);
        let exec = execs().pop().expect("exec");
        let s = snap::decode_for(
            &exec.bytes,
            &crate::snapshot_term::reader_features(),
            |_| None,
        )
        .unwrap();
        let t = s
            .panes
            .iter()
            .find(|p| p.id == pane.0)
            .and_then(|p| p.term.as_ref())
            .unwrap();
        let history = &t.primary.history;
        let blobs = history.iter().filter(|p| p.is_ghostty()).count();
        let rows: u32 = history.iter().map(|p| p.rows).sum();
        let needs = s.meta.requires.contains(&ghostty_pages_requirement());
        if ghostty {
            // All but the page the screen starts in, which Ghostty keeps
            // resident.
            assert!(
                blobs > 10 && rows < page_rows,
                "{binary:?}: {blobs} blobs, {rows} rows"
            );
            assert!(needs && s.meta.ghostty_patches == PATCH_STACK);
        } else {
            assert!(
                blobs == 0 && rows > 7_900,
                "{binary:?}: {blobs} blobs, {rows} rows"
            );
            assert!(!needs, "{:?}", s.meta.requires);
        }
        let back = term_from_snapshot(t, &s.meta.ghostty_patches).unwrap();
        assert!(capture(&back, all) == shown, "{binary:?}: history differs");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

/// Decisions 6 and 7: a dry run that fails refuses the upgrade with what
/// the binary said, or how it ended; the old server carries on.
#[test]
fn a_failed_dry_run_refuses_and_the_server_carries_on() {
    let (mut server, dir) = setup("fail");
    let said = script(
        &dir,
        "said",
        "echo 'snapshot: newer' >&2; echo 'v2' >&2; exit 3",
    );
    let quiet = script(&dir, "quiet", "exit 4");
    let garbage = dir.join("garbage");
    std::fs::write(&garbage, b"\x7fnot a program").unwrap();
    std::fs::set_permissions(&garbage, std::fs::Permissions::from_mode(0o755)).unwrap();
    for (binary, want) in [
        (said, "snapshot: newer\nv2".to_owned()),
        (
            quiet.clone(),
            format!(
                "{} --vtmux-check-snapshot failed: exit status: 4",
                quiet.display()
            ),
        ),
        (
            garbage.clone(),
            format!("{}: Exec format error", garbage.display()),
        ),
    ] {
        let a = upgrade(&mut server, &dir, &binary);
        assert_eq!(
            (a.err.as_str(), a.exit),
            (format!("{want}\n").as_str(), Some(1))
        );
        assert!(server.upgrade.pending.is_none());
    }
    assert!(execs().is_empty());
    assert!(has_session(&server, "a"));
    st::ok(
        &mut server,
        &dir,
        &["new-window", "-d", "-t", "a", "cat", "-"],
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// Decision 10: a `run-shell` job still running holds the upgrade; at the
/// end of the wait it is refused, naming the job.
#[test]
fn a_running_job_refuses_the_upgrade_after_the_wait() {
    let (mut server, dir) = setup("job");
    st::ok(&mut server, &dir, &["run-shell", "-b", "sleep 30"]);
    let b = passing(&dir);
    let b = b.to_str().unwrap();
    let (id, mut peer) = st::connect(
        &mut server,
        st::hello(&["vtmux-upgrade", "-b", b], &dir, None, ControlMode::None),
    );
    server.upgrade_turn(Instant::now());
    assert!(server.upgrade.pending.is_some(), "still waiting");
    assert_eq!(st::messages(&mut server, id, &mut peer), []);
    server.upgrade_turn(Instant::now() + super::PREFLIGHT_WAIT);
    server.cmdq_process();
    for (c, code) in std::mem::take(&mut server.pending_exits) {
        server.finish_client(c, code);
    }
    let err: Vec<u8> = st::messages(&mut server, id, &mut peer)
        .into_iter()
        .filter_map(|m| match m {
            ServerMsg::Stderr(b) => Some(b),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(
        String::from_utf8(err).unwrap(),
        "still running after 3 s: job 'sleep 30'\n"
    );
    assert!(execs().is_empty() && server.upgrade.pending.is_none());
    server.jobs_kill_all();
    std::fs::remove_dir_all(dir).unwrap();
}

/// Decision 26: a command client still being served (here one that has
/// not said `Hello`) holds the upgrade; once it has gone, the upgrade
/// runs. A second request meanwhile is refused.
#[test]
fn a_client_in_flight_holds_the_upgrade_until_it_goes() {
    let (mut server, dir) = setup("inflight");
    let (ours, other) = mio::net::UnixStream::pair().unwrap();
    let other_id = ClientId(server.next_client);
    server.next_client += 1;
    server
        .clients
        .insert(other_id, super::super::Client::new(ours, 0));
    let binary = passing(&dir);
    let (id, mut peer) = handshake(&mut server, &binary);
    server.upgrade_turn(Instant::now());
    assert!(server.upgrade.pending.is_some());
    let (second, mut second_peer) = handshake(&mut server, &binary);
    assert_eq!(
        results(&mut server, second, &mut second_peer),
        [(false, "an upgrade is already running".to_owned())]
    );
    drop(other);
    server.drop_client(other_id);
    turns(&mut server, Instant::now());
    assert_eq!(
        results(&mut server, id, &mut peer),
        [(false, format!("{}: test exec", binary.display()))]
    );
    assert_eq!(execs().len(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

/// A requester that goes while its upgrade waits takes the upgrade with
/// it.
#[test]
fn an_upgrade_whose_requester_went_is_dropped() {
    let (mut server, dir) = setup("gone");
    st::ok(&mut server, &dir, &["run-shell", "-b", "sleep 30"]);
    let (id, _peer) = handshake(&mut server, &passing(&dir));
    server.upgrade_turn(Instant::now());
    assert!(server.upgrade.pending.is_some());
    server.drop_client(id);
    server.jobs_kill_all();
    server.upgrade_turn(Instant::now() + super::PREFLIGHT_WAIT);
    assert!(server.upgrade.pending.is_none() && execs().is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

/// An attached client is carried: its socket is kept across the exec.
#[test]
fn attached_clients_are_kept_across_the_exec() {
    let (mut server, dir) = setup("attached");
    let (term, _term_peer) = st::attach(&mut server, &dir, "a", &[]);
    let (_id, _peer) = handshake(&mut server, &passing(&dir));
    turns(&mut server, Instant::now());
    let exec = execs().pop().expect("exec");
    let fd = server.clients[&term].stream.as_raw_fd();
    assert!(exec.keep.contains(&fd));
    std::fs::remove_dir_all(dir).unwrap();
}

/// The command runs a binary as the server's owner: a read-only client
/// may not run it, whether as a command (the read-only gate) or as a
/// control client's line (which the gate skips).
#[test]
fn a_read_only_client_may_not_upgrade() {
    use crate::acl::Subject;
    let (mut server, dir) = setup("ro");
    let uid = rustix::process::getuid().as_raw();
    server.acl.set_read_only(Subject::User(uid), true);
    let binary = passing(&dir);
    let a = upgrade(&mut server, &dir, &binary);
    assert_eq!((a.err.as_str(), a.exit), ("client is read-only\n", Some(1)));
    let (control, mut peer) = st::connect(
        &mut server,
        st::hello(&["attach", "-t", "a"], &dir, None, ControlMode::Control),
    );
    let line = format!("vtmux-upgrade -b {}\n", binary.display());
    server.control_stdin(control, line.as_bytes());
    let out: Vec<u8> = st::messages(&mut server, control, &mut peer)
        .into_iter()
        .filter_map(|m| match m {
            ServerMsg::Stdout(b) => Some(b),
            _ => None,
        })
        .flatten()
        .collect();
    assert!(String::from_utf8_lossy(&out).contains("\nclient is read-only\n%error"));
    assert!(server.upgrade.pending.is_none() && execs().is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

/// Decision 22: without `-b` the server moves onto the path it was
/// started as, resolved when asked: relinking that path to a new binary
/// upgrades to the new one.
#[test]
fn the_default_binary_follows_a_relink_of_the_start_path() {
    let (mut server, dir) = setup("relink");
    let old = passing(&dir);
    let new = script(&dir, "new", "cat > /dev/null");
    let link = dir.join("tmux");
    std::os::unix::fs::symlink(&old, &link).unwrap();
    server.start_binary = Some(link.clone());
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&new, &link).unwrap();
    command(&mut server, &dir, &["vtmux-upgrade"], Instant::now());
    let exec = execs().pop().expect("exec");
    assert_eq!(exec.binary, std::fs::canonicalize(&new).unwrap());
    assert!(!dir.join("args").exists(), "the old binary never ran");
    // The start path is carried across an upgrade.
    let rebuilt = st::rebuild(&server, &dir, "b");
    assert_eq!(rebuilt.start_binary, Some(link));
    std::fs::remove_dir_all(dir).unwrap();
}

/// What the server was started as: a path from the directory it was run
/// in, a name from `PATH` (an executable file only), symlinks kept.
#[test]
fn the_start_binary_is_argv0_made_absolute() {
    let dir = st::temp_dir("up-start");
    let (a, b) = (dir.join("a"), dir.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("tmux"), "not executable").unwrap();
    let target = script(&b, "real", "exit 0");
    std::os::unix::fs::symlink(&target, b.join("tmux")).unwrap();
    let path = format!("{}:{}", a.display(), b.display());
    let start = |argv0: &str, path: Option<&str>| {
        super::start_binary(std::ffi::OsStr::new(argv0), path, &dir)
    };
    assert_eq!(start("tmux", Some(&path)), Some(b.join("tmux")));
    assert_eq!(
        start("./bin/vtmux", Some(&path)),
        Some(dir.join("./bin/vtmux"))
    );
    assert_eq!(start("/opt/vtmux", None), Some(PathBuf::from("/opt/vtmux")));
    assert_eq!(start("tmux", Some(&a.display().to_string())), None);
    // Relative entries are from the directory.
    assert_eq!(start("tmux", Some("b")), Some(dir.join("b").join("tmux")));
    std::fs::remove_dir_all(dir).unwrap();
}

/// After the exec, the image that serves tells a carried requester how
/// the upgrade went (`resume.rs`, `answer`): a command client exits, 0 or
/// 1 with why on stderr; a control client gets why as a line and reads
/// its next one; an attached client gets why as a status message.
#[test]
fn a_carried_requester_is_answered_by_the_image_that_serves() {
    let (mut server, dir) = setup("answer");
    let exits = |server: &mut Server, id, peer: &mut mio::net::UnixStream| {
        st::messages(server, id, peer)
            .into_iter()
            .filter_map(|m| match m {
                ServerMsg::Stderr(b) => Some(format!("err {}", String::from_utf8_lossy(&b))),
                ServerMsg::Exit { code, .. } => Some(format!("exit {code}")),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    for (outcome, want) in [
        (Ok(String::new()), vec!["exit 0".to_owned()]),
        (
            Err("live upgrade failed: why".to_owned()),
            vec![
                "err live upgrade failed: why\n".to_owned(),
                "exit 1".to_owned(),
            ],
        ),
    ] {
        // A command client still being served, as the requester is.
        let (id, mut peer) = st::connect(
            &mut server,
            st::hello(&["wait-for", "never"], &dir, None, ControlMode::None),
        );
        server.cmdq_process();
        assert_eq!(exits(&mut server, id, &mut peer), Vec::<String>::new());
        server.upgrade_answer(id, outcome);
        assert_eq!(exits(&mut server, id, &mut peer), want);
    }

    let (control, mut peer) = st::connect(
        &mut server,
        st::hello(&["attach", "-t", "a"], &dir, None, ControlMode::Control),
    );
    let _ = st::messages(&mut server, control, &mut peer);
    // A line held behind the upgrade, as the old image left it.
    server
        .clients
        .get_mut(&control)
        .and_then(|c| c.control.as_mut())
        .unwrap()
        .inbuf
        .extend_from_slice(b"display -p held\n");
    server.upgrade_answer(control, Err("live upgrade failed: why".to_owned()));
    server.cmdq_process();
    let out: Vec<u8> = st::messages(&mut server, control, &mut peer)
        .into_iter()
        .filter_map(|m| match m {
            ServerMsg::Stdout(b) => Some(b),
            _ => None,
        })
        .flatten()
        .collect();
    let out = String::from_utf8_lossy(&out);
    assert!(
        out.starts_with("live upgrade failed: why\n%begin "),
        "{out}"
    );
    assert!(out.contains("\nheld\n%end "), "{out}");

    let (term, _term_peer) = st::attach(&mut server, &dir, "a", &[]);
    server.upgrade_answer(term, Err("live upgrade failed: why".to_owned()));
    let shown = "/dev/vtmux-test-tty message: live upgrade failed: why";
    let logged = server.messages_newest_first().next().unwrap().text.clone();
    assert_eq!(logged, shown);
    server.upgrade_answer(term, Ok(String::new()));
    assert_eq!(
        server.messages_newest_first().next().unwrap().text,
        shown,
        "a success says nothing"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// An attached terminal client named `tty` on `session`, whose binary
/// follows upgrades or (`follows` false) is from before they did.
fn attach_as(
    server: &mut Server,
    dir: &Path,
    session: &str,
    tty: &str,
    follows: bool,
) -> (ClientId, mio::net::UnixStream) {
    let mut h = st::hello(
        &["attach-session", "-t", session],
        dir,
        Some(vtmux_core::protocol::TtyInfo {
            name: tty.into(),
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
            features: Vec::new(),
        }),
        ControlMode::None,
    );
    h.ext.follows_upgrade = follows;
    let out = st::connect(server, h);
    server.pending_exits.clear();
    out
}

/// Client `id`'s peer has closed its end: wait for the server to see the
/// end of the stream (a child another test thread forks may hold a copy
/// of the peer until it execs).
fn gone(server: &mut Server, id: ClientId) {
    let end = Instant::now() + std::time::Duration::from_secs(10);
    loop {
        server.client_readable(id);
        if server.clients[&id].reattach.is_some_and(|r| r.gone) {
            return;
        }
        assert!(Instant::now() < end, "{:?}", server.clients[&id].reattach);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// The notices in `peer`'s stream: (binary, token).
fn notices(
    server: &mut Server,
    id: ClientId,
    peer: &mut mio::net::UnixStream,
) -> Vec<(PathBuf, String)> {
    st::messages(server, id, peer)
        .into_iter()
        .filter_map(|m| match m {
            ServerMsg::UpgradeNotice { binary, token } => Some((binary, token)),
            _ => None,
        })
        .collect()
}

/// Step 6 (decisions 4 and 21): once the dry run passes, each attached
/// client that follows upgrades is sent the binary and the token its
/// record carries in the snapshot; one from before (no
/// `follows_upgrade`) gets nothing and carries no token. An exec that
/// returns leaves the follower waiting to re-attach, to this image.
#[test]
fn followers_are_told_to_re_execute_with_the_token_the_snapshot_carries() {
    let (mut server, dir) = setup("notice");
    let (f, mut f_peer) = attach_as(&mut server, &dir, "a", "/dev/f", true);
    let (o, mut o_peer) = attach_as(&mut server, &dir, "a", "/dev/o", false);
    let _ = st::messages(&mut server, f, &mut f_peer);
    let _ = st::messages(&mut server, o, &mut o_peer);
    let binary = passing(&dir);
    upgrade(&mut server, &dir, &binary);
    let s = snap::decode(&execs().pop().expect("exec").bytes).unwrap();
    let token = |id: ClientId| {
        s.clients
            .iter()
            .find(|c| c.id as usize == id.0)
            .map(|c| c.token)
            .unwrap()
    };
    assert_ne!(token(f), 0);
    assert_eq!(token(o), 0);
    let sent = notices(&mut server, f, &mut f_peer);
    assert_eq!(sent, [(binary, format!("{:016x}", token(f)))]);
    assert!(vtmux_core::protocol::is_upgrade_token(&sent[0].1));
    assert_eq!(notices(&mut server, o, &mut o_peer), []);
    let r = server.clients[&f].reattach.expect("waiting to re-attach");
    assert!(r.deadline.is_some() && !r.gone);
    std::fs::remove_dir_all(dir).unwrap();
}

/// A dry run that fails sends no notice, and the tokens it would have
/// carried are void.
#[test]
fn a_failed_dry_run_sends_no_notice() {
    let (mut server, dir) = setup("nonotice");
    let (f, mut f_peer) = attach_as(&mut server, &dir, "a", "/dev/f", true);
    let _ = st::messages(&mut server, f, &mut f_peer);
    upgrade(&mut server, &dir, &script(&dir, "no", "exit 2"));
    assert_eq!(notices(&mut server, f, &mut f_peer), []);
    assert!(server.clients[&f].reattach.is_none());
    std::fs::remove_dir_all(dir).unwrap();
}

/// Decision 21: the client re-executed (its old connection closed) and
/// re-attached with its token is bound back to its record: the same id,
/// creation time and key table, no `client-*` hook and no `%client-*`
/// notification to a control client; it is sent `Ready`, then its
/// terminal started again, and nothing until then. A token no record
/// holds is told to attach again.
#[test]
fn a_re_attach_rebinds_the_record_and_fires_nothing() {
    let (mut server, dir) = setup("rebind");
    st::ok(&mut server, &dir, &["new-session", "-d", "-s", "b"]);
    st::ok(&mut server, &dir, &["set", "-g", "@fired", "0"]);
    for hook in [
        "client-attached",
        "client-detached",
        "client-session-changed",
        "client-resized",
        "client-active",
    ] {
        st::ok(
            &mut server,
            &dir,
            &["set-hook", "-g", hook, &format!("set -ga @fired {hook}")],
        );
    }
    let (f, mut f_peer) = attach_as(&mut server, &dir, "a", "/dev/f", true);
    let (control, mut c_peer) = st::connect(
        &mut server,
        st::hello(&["attach", "-t", "b"], &dir, None, ControlMode::Control),
    );
    server.cmdq_process();
    let _ = st::messages(&mut server, f, &mut f_peer);
    let _ = st::messages(&mut server, control, &mut c_peer);
    let fired = st::run(&mut server, &dir, &["show", "-gv", "@fired"]);
    server.cmdq_process();
    let created = server.clients[&f].created_secs;

    server.upgrade_tokens();
    server.upgrade_notify(&dir.join("new"));
    let token = notices(&mut server, f, &mut f_peer).pop().unwrap().1;
    // The client execs: its old connection closes, the record stays.
    drop(f_peer);
    gone(&mut server, f);
    // Nothing is drawn for it meanwhile.
    server.redraw_client(f);
    server.render_clients();

    let mut h = st::hello(
        &[],
        &dir,
        Some(vtmux_core::protocol::TtyInfo {
            name: "/dev/f".into(),
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
            features: Vec::new(),
        }),
        ControlMode::None,
    );
    h.ext.follows_upgrade = true;
    h.ext.reattach = Some(token.clone());
    let (new, mut peer) = st::connect(&mut server, h.clone());
    assert!(
        !server.clients.contains_key(&new),
        "bound to the old record"
    );
    let c = &server.clients[&f];
    assert!(c.reattach.is_none() && c.attached.is_some());
    assert_eq!(c.created_secs, created);
    let got = st::messages(&mut server, f, &mut peer);
    assert_eq!(got.first(), Some(&ServerMsg::Ready), "{got:?}");
    let Some(ServerMsg::Frame(start)) = got.get(1) else {
        panic!("{got:?}");
    };
    assert!(start.ends_with(b"\x1b[?996n"), "tty_start_tty first");
    // The client acknowledges what it wrote; then the redraw.
    server.on_message(
        f,
        vtmux_core::protocol::ClientMsg::FrameAck { seq: u64::MAX },
    );
    server.render_clients();
    server.cmdq_process();
    let got = st::messages(&mut server, f, &mut peer);
    assert!(
        // Every row is drawn, blank ones too.
        matches!(got.as_slice(), [ServerMsg::Frame(b)]
            if (1..24).all(|row| String::from_utf8_lossy(b).contains(&format!("\x1b[{row};1H")))),
        "the redraw: {got:?}"
    );
    assert_eq!(
        st::run(&mut server, &dir, &["show", "-gv", "@fired"]),
        fired,
        "no hook"
    );
    server.cmdq_process();
    let lines: String = st::messages(&mut server, control, &mut c_peer)
        .into_iter()
        .filter_map(|m| match m {
            ServerMsg::Stdout(b) => Some(String::from_utf8_lossy(&b).into_owned()),
            _ => None,
        })
        .collect();
    assert!(!lines.contains("%client-"), "{lines}");

    // The token is used up.
    let (again, mut again_peer) = st::connect(&mut server, h);
    assert_eq!(
        st::messages(&mut server, again, &mut again_peer),
        [ServerMsg::Exit {
            code: 1,
            message: Some("server upgraded: re-attach".into()),
            hup: false,
        }]
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// In the image that serves after the exec: a client carried with a
/// token waits to re-attach; one without (decision 25) is detached with
/// `[server upgraded: re-attach]` and named in the warnings. A follower
/// that never re-attaches is given up on at its deadline: detached the
/// same way while its old connection is open (it ignored its notice),
/// lost once that has closed.
#[test]
fn clients_that_cannot_follow_are_detached_and_named() {
    let (mut server, dir) = setup("unfollowing");
    let (f, mut f_peer) = attach_as(&mut server, &dir, "a", "/dev/f", true);
    let (g, mut g_peer) = attach_as(&mut server, &dir, "a", "/dev/g", true);
    let (o, mut o_peer) = attach_as(&mut server, &dir, "a", "/dev/o", false);
    for (id, peer) in [(f, &mut f_peer), (g, &mut g_peer), (o, &mut o_peer)] {
        let _ = st::messages(&mut server, id, peer);
    }
    server.upgrade_tokens();
    let mut new = st::rebuild(&server, &dir, "b");
    new.detach_unfollowing();
    assert_eq!(
        new.upgrade.warnings,
        [
            "warning: client /dev/o can't follow an upgrade: detached with \
          [server upgraded: re-attach]"
        ]
    );
    let exit = |m: &[ServerMsg]| {
        m.iter()
            .filter_map(|m| match m {
                ServerMsg::Exit { code, message, .. } => Some((*code, message.clone())),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let detached = vec![(1, Some("server upgraded: re-attach".to_owned()))];
    assert_eq!(exit(&st::messages(&mut new, o, &mut o_peer)), detached);
    assert!(new.clients[&f].reattach.is_some() && new.clients[&g].reattach.is_some());

    // g's old connection closes (it re-executes) and it never comes back;
    // f's stays open: it ignored its notice.
    drop(g_peer);
    gone(&mut new, g);
    // Its process still runs (it would be given up on at once otherwise).
    new.clients.get_mut(&g).unwrap().hello.as_mut().unwrap().pid = std::process::id();
    let deadline = new
        .clients
        .values()
        .filter_map(|c| c.reattach?.deadline)
        .min()
        .unwrap();
    new.reattach_timers(
        deadline
            .checked_sub(std::time::Duration::from_millis(1))
            .unwrap(),
    );
    assert!(new.clients.contains_key(&f) && new.clients.contains_key(&g));
    new.reattach_timers(deadline);
    assert!(!new.clients.contains_key(&g), "lost");
    assert_eq!(exit(&st::messages(&mut new, f, &mut f_peer)), detached);
    let logged: Vec<String> = new
        .messages_newest_first()
        .take(2)
        .map(|m| m.text.clone())
        .collect();
    assert!(
        logged.contains(
            &"warning: client /dev/f did not act on the upgrade: detached with \
              [server upgraded: re-attach]"
                .to_owned()
        ),
        "{logged:?}"
    );
    drop(new);
    std::fs::remove_dir_all(dir).unwrap();
}

/// The finding behind `medium_reexec_teardown`: a client that does not
/// come back (its exec fails, the new binary can't re-attach, its token
/// is refused) must leave its terminal as a detach does. So step 6 sends
/// each client the teardown (`tty_stop_tty`: the alternate screen left,
/// the title popped, mouse, bracketed paste, focus reports and keypad
/// off) just before its notice, which the client writes out before the
/// exec; nothing is drawn for it after.
#[test]
fn the_notice_comes_after_the_terminal_is_restored() {
    let (mut server, dir) = setup("teardown");
    let (f, mut f_peer) = attach_as(&mut server, &dir, "a", "/dev/f", true);
    let _ = st::messages(&mut server, f, &mut f_peer);
    server.upgrade_tokens();
    server.upgrade_notify(&dir.join("new"));
    let got = st::messages(&mut server, f, &mut f_peer);
    let [ServerMsg::Frame(teardown), ServerMsg::UpgradeNotice { .. }] = got.as_slice() else {
        panic!("{got:?}");
    };
    let teardown = String::from_utf8_lossy(teardown);
    for off in [
        "\x1b[?1049l",
        "\x1b[23;0;0t",
        "\x1b[?1000l",
        "\x1b[?1006l",
        "\x1b[?2004l",
        "\x1b[?1004l",
        "\x1b[?1l",
        "\x1b>",
    ] {
        assert!(teardown.contains(off), "{off:?} in {teardown:?}");
    }
    server.redraw_client(f);
    server.render_clients();
    assert_eq!(st::messages(&mut server, f, &mut f_peer), []);
    std::fs::remove_dir_all(dir).unwrap();
}

/// A process id that is no process's any more.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// The image that serves holds the requester's answer until every client
/// told to re-attach has, or has been given up on, and names those that
/// did not come back: here one whose process exited after its old
/// connection closed (a binary that can't re-attach), given up on at once
/// rather than at its deadline. The give-up is in the message log too.
#[test]
fn the_answer_waits_for_the_clients_re_attaching_and_names_the_lost() {
    use std::io::Read as _;
    let (mut server, dir) = setup("later");
    let (f, mut f_peer) = attach_as(&mut server, &dir, "a", "/dev/f", true);
    let (g, mut g_peer) = attach_as(&mut server, &dir, "a", "/dev/g", true);
    let (o, mut o_peer) = attach_as(&mut server, &dir, "a", "/dev/o", false);
    for (id, peer) in [(f, &mut f_peer), (g, &mut g_peer), (o, &mut o_peer)] {
        let _ = st::messages(&mut server, id, peer);
    }
    server.upgrade_tokens();
    let token = server.clients[&f].reattach.unwrap().text();
    let mut new = st::rebuild(&server, &dir, "b");
    new.detach_unfollowing();
    let (ours, mut theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    theirs.set_nonblocking(true).unwrap();
    let answered = |theirs: &mut std::os::unix::net::UnixStream| {
        let mut buf = Vec::new();
        let _ = theirs.read_to_end(&mut buf);
        (!buf.is_empty()).then(|| {
            vtmux_core::protocol::read_msg::<ServerMsg>(&mut buf.as_slice())
                .unwrap()
                .unwrap()
        })
    };
    new.upgrade_answer_later(super::LaterAnswer {
        requester: snap::Requester::Handshake(ours.as_raw_fd()),
        handshake: Some(OwnedFd::from(ours)),
        outcome: Ok(()),
    });
    assert_eq!(answered(&mut theirs), None, "f and g are on their way");
    // Meanwhile another upgrade would lose the answer: refused.
    assert_eq!(
        new.upgrade_start(super::Requester::Handshake(f), passing(&dir)),
        Err("the last upgrade is still waiting for its clients to re-attach".to_owned())
    );

    // g re-executed and its process exited: not coming back.
    drop(g_peer);
    gone(&mut new, g);
    new.clients.get_mut(&g).unwrap().hello.as_mut().unwrap().pid = dead_pid();
    let deadline = new.reattach_deadline().unwrap();
    assert!(deadline < Instant::now() + super::REATTACH_WAIT / 2);
    new.reattach_timers(Instant::now());
    new.upgrade_answer_due();
    assert!(!new.clients.contains_key(&g), "given up on");
    assert_eq!(answered(&mut theirs), None, "f is on its way");
    let lost = "warning: client /dev/g did not re-attach after the upgrade: its session is \
                still there, attach again";
    assert_eq!(new.messages_newest_first().next().unwrap().text, lost);

    // f comes back: the answer goes, naming o and g.
    drop(f_peer);
    gone(&mut new, f);
    let mut h = st::hello(
        &[],
        &dir,
        Some(vtmux_core::protocol::TtyInfo {
            name: "/dev/f".into(),
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
            features: Vec::new(),
        }),
        ControlMode::None,
    );
    h.ext.follows_upgrade = true;
    h.ext.reattach = Some(token);
    let (_new_f, _new_f_peer) = st::connect(&mut new, h);
    assert!(new.clients[&f].reattach.is_none());
    new.upgrade_answer_due();
    assert_eq!(
        answered(&mut theirs),
        Some(ServerMsg::UpgradeResult {
            ok: true,
            message: format!(
                "warning: client /dev/o can't follow an upgrade: detached with \
                 [server upgraded: re-attach]\n{lost}"
            ),
        })
    );
    assert!(new.upgrade.answer.is_none() && new.upgrade.warnings.is_empty());
    drop(new);
    std::fs::remove_dir_all(dir).unwrap();
}

/// A re-attach is let in as the access list says now: one from a peer
/// made read-only since gets a read-only client, whatever the record's
/// flags were; one from another user is refused and leaves the record
/// waiting with its token.
#[test]
fn a_re_attach_is_as_the_access_list_says_now() {
    use crate::acl::Subject;
    let (mut server, dir) = setup("reacl");
    let (f, mut f_peer) = attach_as(&mut server, &dir, "a", "/dev/f", true);
    let _ = st::messages(&mut server, f, &mut f_peer);
    assert!(!server.clients[&f].flags.read_only);
    server.upgrade_tokens();
    server.upgrade_notify(&dir.join("new"));
    let token = notices(&mut server, f, &mut f_peer).pop().unwrap().1;
    drop(f_peer);
    gone(&mut server, f);
    let mut h = st::hello(
        &[],
        &dir,
        Some(vtmux_core::protocol::TtyInfo {
            name: "/dev/f".into(),
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
            features: Vec::new(),
        }),
        ControlMode::None,
    );
    h.ext.follows_upgrade = true;
    h.ext.reattach = Some(token);

    // Another user's connection presenting the token.
    let (uid, gid) = server.clients[&f].peer.unwrap();
    server.clients.get_mut(&f).unwrap().peer = Some((uid + 1, gid));
    let (other, mut other_peer) = st::connect(&mut server, h.clone());
    let refused = st::messages(&mut server, other, &mut other_peer);
    assert!(
        matches!(refused.last(), Some(ServerMsg::Exit { code: 1, message: Some(m), .. })
            if m == "access not allowed"),
        "{refused:?}"
    );
    assert!(server.clients[&f].reattach.is_some(), "still waiting");
    server.clients.get_mut(&f).unwrap().peer = Some((uid, gid));

    // Its own user, made read-only meanwhile.
    server.acl.set_read_only(Subject::User(uid), true);
    let (new, _peer) = st::connect(&mut server, h);
    assert!(
        !server.clients.contains_key(&new),
        "bound to the old record"
    );
    let c = &server.clients[&f];
    assert!(c.reattach.is_none() && c.attached.is_some());
    assert!(c.flags.read_only, "read-only as the access list says");
    std::fs::remove_dir_all(dir).unwrap();
}

/// A prompt a key binding opened (`command-prompt`, which waits in the
/// client's queue until it closes, cmd-command-prompt.c) holds the
/// upgrade like any command in flight; the refusal says what to do.
#[test]
fn an_open_prompt_refuses_the_upgrade_saying_what_to_close() {
    let (mut server, dir) = setup("prompt");
    let (term, mut term_peer) = st::attach(&mut server, &dir, "a", &[]);
    let _ = st::messages(&mut server, term, &mut term_peer);
    // The prefix key, then `:`.
    server.on_client_input(term, b"\x02:").unwrap();
    server.cmdq_process();
    assert!(
        server.clients[&term].status.prompt().is_some(),
        "prompt open"
    );
    let binary = passing(&dir);
    let b = binary.to_str().unwrap();
    let a = command(
        &mut server,
        &dir,
        &["vtmux-upgrade", "-b", b],
        Instant::now() + super::PREFLIGHT_WAIT * 2,
    );
    assert_eq!(
        (a.err.as_str(), a.exit),
        (
            "still running after 3 s: client /dev/vtmux-test-tty has a prompt open: close it, \
             then run vtmux-upgrade again\n",
            Some(1)
        )
    );
    assert!(execs().is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

/// A binary of this patch stack whose `n`th dry run says it started in
/// `dir/started<n>`, then passes once `dir/release<n>` exists (or after
/// 20 s).
fn gated(dir: &Path) -> PathBuf {
    let (runs, at) = (dir.join("runs"), dir.display());
    script(
        dir,
        "gated",
        &format!(
            "case \"$1\" in\n\
             --vtmux-patch-stack) echo {}; exit 0;;\n\
             --vtmux-check-snapshot) ;;\n\
             *) exit 1;;\n\
             esac\n\
             echo x >>'{}'; n=$(wc -l <'{}'); n=$((n))\n\
             : >\"{at}/started$n\"\n\
             i=0\n\
             while [ ! -e \"{at}/release$n\" ] && [ $i -lt 400 ]; do sleep 0.05; i=$((i+1)); done\n\
             exit 0",
            crate::snapshot_term::PATCH_STACK,
            runs.display(),
            runs.display(),
        ),
    )
}

/// The loop's upgrade turns until the `n`th dry run of [`gated`] has
/// started: then it runs while the loop has its turn back.
fn started(server: &mut Server, dir: &Path, n: usize) {
    let end = Instant::now() + std::time::Duration::from_secs(20);
    while !dir.join(format!("started{n}")).exists() {
        server.upgrade_turn(Instant::now());
        assert!(Instant::now() < end, "dry run {n} did not start");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        server.upgrade_dry_run_pid().is_some(),
        "the loop got its turn back while the dry run runs"
    );
}

/// The upgrade asked over the handshake onto [`gated`], its first dry run
/// left running: the requester's connection.
fn checking(server: &mut Server, dir: &Path) -> (ClientId, mio::net::UnixStream) {
    let asked = handshake(server, &gated(dir));
    started(server, dir, 1);
    asked
}

/// The `n`th dry run of [`gated`] goes on.
fn release(dir: &Path, n: usize) {
    std::fs::write(dir.join(format!("release{n}")), "").unwrap();
}

/// The dry run runs while the server serves (`review_upgrade_cmd_2`,
/// `low_dry_run_blocks_loop`): the loop turn that starts it returns, and
/// commands are run meanwhile. Once it has passed, the snapshot the exec
/// gets is taken again, so what changed meanwhile is in it, not lost.
#[test]
fn the_server_serves_while_the_dry_run_runs_and_the_exec_gets_what_changed() {
    let (mut server, dir) = setup("serves");
    let (id, mut peer) = checking(&mut server, &dir);
    st::ok(
        &mut server,
        &dir,
        &["new-window", "-d", "-t", "a", "-n", "during", "cat", "-"],
    );
    assert!(execs().is_empty(), "no exec before the dry run has passed");
    release(&dir, 1);
    turns(&mut server, Instant::now());
    let binary = dir.join("gated");
    assert_eq!(
        results(&mut server, id, &mut peer),
        [(false, format!("{}: test exec", binary.display()))]
    );
    let s = snap::decode(&execs().pop().expect("exec").bytes).unwrap();
    let names: Vec<&str> = s.windows.iter().map(|w| w.name.as_str()).collect();
    assert!(names.contains(&"during"), "{names:?}");
    std::fs::remove_dir_all(dir).unwrap();
}

/// A snapshot taken after the dry run that requires a feature the checked
/// one did not gets a dry run of its own: the binary was never shown it
/// reads that. Here a pane's first history pages in Ghostty's form (a
/// pane printing through the dry run, `control_clients_stream_on_...`
/// found it), then an auto-hide scrollbar shown: three dry runs, one
/// exec of the last snapshot, one notice.
#[test]
fn a_feature_needed_since_the_dry_run_gets_a_dry_run_of_its_own() {
    use crate::snapshot_term::ghostty_pages_requirement;
    let (mut server, dir) = setup("needs");
    let (f, mut f_peer) = attach_as(&mut server, &dir, "a", "/dev/f", true);
    let _ = st::messages(&mut server, f, &mut f_peer);
    let (id, mut peer) = checking(&mut server, &dir);
    assert!(server.clients[&f].reattach.is_some(), "the dry run's token");
    let p = *server.model.panes.keys().next().unwrap();
    let term = &mut server.model.panes.get_mut(&p).unwrap().term;
    for i in 0..8000 {
        term.feed(format!("line {i}\r\n").as_bytes());
    }
    release(&dir, 1);
    started(&mut server, &dir, 2);
    server.model.panes.get_mut(&p).unwrap().sb.visible = true;
    release(&dir, 2);
    started(&mut server, &dir, 3);
    assert!(execs().is_empty() && notices(&mut server, f, &mut f_peer).is_empty());
    release(&dir, 3);
    turns(&mut server, Instant::now());
    let binary = dir.join("gated");
    assert_eq!(
        results(&mut server, id, &mut peer),
        [(false, format!("{}: test exec", binary.display()))]
    );
    let s = snap::decode_for(
        &execs().pop().expect("exec").bytes,
        &crate::snapshot_term::reader_features(),
        |_| None,
    )
    .unwrap();
    assert!(
        s.meta.requires.contains(&ghostty_pages_requirement())
            && s.meta
                .requires
                .contains(&snap::SCROLLBAR_FEATURE.to_string()),
        "{:?}",
        s.meta.requires
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("runs")).unwrap(),
        "x\nx\nx\n"
    );
    assert_eq!(notices(&mut server, f, &mut f_peer).len(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

/// A requester that goes while the dry run runs takes the upgrade with
/// it: the dry run is killed, the tokens void.
#[test]
fn a_requester_gone_during_the_dry_run_kills_it() {
    let (mut server, dir) = setup("gonedry");
    let (f, mut f_peer) = attach_as(&mut server, &dir, "a", "/dev/f", true);
    let _ = st::messages(&mut server, f, &mut f_peer);
    let (id, _peer) = checking(&mut server, &dir);
    let pid = server.upgrade_dry_run_pid().unwrap();
    server.drop_client(id);
    server.upgrade_turn(Instant::now());
    assert!(server.upgrade.pending.is_none() && execs().is_empty());
    assert!(server.clients[&f].reattach.is_none(), "the token is void");
    // Killed and reaped: no such process any more.
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
    std::fs::remove_dir_all(dir).unwrap();
}
