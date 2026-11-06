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
fn record_exec(binary: &Path, file: OwnedFd, keep: &[RawFd]) -> io::Error {
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

/// Run `words` as a command client, then the loop's upgrade turn at `at`
/// and the queues, until the client is told to exit.
fn command(server: &mut Server, dir: &Path, words: &[&str], at: Instant) -> Answer {
    let (id, mut peer) = st::connect(server, st::hello(words, dir, None, ControlMode::None));
    let mut answer = Answer::default();
    for _ in 0..3 {
        server.upgrade_turn(at);
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
    server.upgrade_turn(Instant::now());
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
    server.upgrade_turn(Instant::now());
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
