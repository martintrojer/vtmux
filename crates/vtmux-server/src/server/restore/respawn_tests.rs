//! Respawning a restored pane (live-upgrade spec, decisions 31 and 32,
//! *Respawning a restored pane*): provenance, the hint, the screen kept,
//! the Enter key and `vtmux-persist-respawn auto`.

use std::path::{Path, PathBuf};

use vtmux_core::ids::PaneId;
use vtmux_core::protocol::ServerMsg;
use vtmux_snapshot as snap;

use super::super::persist::Mode;
use super::super::snapshot::tests::{attach, listener, messages, ok, run, temp_dir};
use super::super::{ClientId, Server, ServerConfig};
use super::{ConfigSays, prepare, rebuild};

/// The checkpoint's write time in these tests (epoch seconds).
const WRITTEN: i64 = 1_790_000_000;

/// A test's directory (its sockets, its config, its shell), removed when
/// the value goes, however many one test makes.
struct TestDir(PathBuf);

impl std::ops::Deref for TestDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A server with session `a` (40x6, `cat -`, `status off`) set up by
/// `setup`, each pane fed its bytes, checkpointed `full`, then rebuilt from
/// that checkpoint as a cold restore builds it. Its `default-shell` is a
/// script running `cat -`: a login shell would read the host's
/// `/etc/profile`.
fn restored(tag: &str, setup: &[&[&str]], feed: &[(u32, &[u8])]) -> (Server, TestDir) {
    restored_with(tag, setup, feed, None)
}

/// [`restored`] with `conf` as the config the rebuilt server runs.
fn restored_with(
    tag: &str,
    setup: &[&[&str]],
    feed: &[(u32, &[u8])],
    conf: Option<&str>,
) -> (Server, TestDir) {
    let dir = TestDir(temp_dir(tag));
    let shell = dir.join("shell");
    std::fs::write(&shell, "#!/bin/sh\nexec cat -\n").unwrap();
    std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let shell = shell.to_str().unwrap().to_owned();
    let (l, socket) = listener(&dir, "s");
    let mut server = Server::new(l, socket, ServerConfig::default()).unwrap();
    for words in [
        &["set", "-g", "default-shell", &shell][..],
        &["set", "-g", "status", "off"],
        &[
            "new-session",
            "-d",
            "-s",
            "a",
            "-x",
            "40",
            "-y",
            "6",
            "cat",
            "-",
        ],
    ]
    .iter()
    .chain(setup)
    {
        ok(&mut server, &dir, words);
    }
    for (p, bytes) in feed {
        let pane = server.model.panes.get_mut(&PaneId(*p)).unwrap();
        pane.term.feed(bytes);
    }
    let mut s = server.checkpoint_snapshot(Mode::Full);
    s.meta.written_us = WRITTEN * 1_000_000 + 999;
    drop(server);
    let (l, socket) = listener(&dir, "s2");
    let config = ServerConfig {
        config_files: conf
            .map(|text| {
                let path = dir.join("conf");
                std::fs::write(&path, text).unwrap();
                vec![path]
            })
            .unwrap_or_default(),
        ..ServerConfig::default()
    };
    let s = prepare(s, &ConfigSays::default(), &socket, &config);
    let mut server = rebuild(l, config, &s).unwrap();
    // As `restore::start` marks a cold restore.
    server.persist.auto_pass = crate::restored::AutoPass::Due;
    (server, dir)
}

/// The visible rows of pane `p` (`capture-pane -p`).
fn rows(server: &mut Server, dir: &Path, p: &str) -> Vec<String> {
    let out = run(server, dir, &["capture-pane", "-p", "-t", p]);
    out.lines()
        .skip(1)
        .take_while(|l| *l != "--stderr--")
        .map(str::to_owned)
        .collect()
}

/// One format of pane `p`.
fn display(server: &mut Server, dir: &Path, p: &str, f: &str) -> String {
    let out = run(server, dir, &["display", "-p", "-t", p, f]);
    out.lines().nth(1).unwrap_or_default().to_owned()
}

/// An attached client's terminal: the frames it was sent, drawn.
struct Screen {
    id: ClientId,
    peer: mio::net::UnixStream,
    term: vtmux_vt::PaneTerm,
}

impl Screen {
    fn attach(server: &mut Server, dir: &Path) -> Self {
        let (id, peer) = attach(server, dir, "a", &[]);
        Self {
            id,
            peer,
            term: vtmux_vt::PaneTerm::new(80, 24, 0),
        }
    }

    /// The client's screen after a render, row by row.
    fn rows(&mut self, server: &mut Server) -> Vec<String> {
        // The attach's first frame starts the terminal; the next draws.
        for _ in 0..2 {
            server.check_window_resizes();
            server.render_clients();
            for m in messages(server, self.id, &mut self.peer) {
                if let ServerMsg::Frame(b) = m {
                    self.term.feed(&b);
                }
            }
            server
                .clients
                .get_mut(&self.id)
                .unwrap()
                .unacked
                .ack(u64::MAX);
        }
        let text = vtmux_vt::capture(&self.term, vtmux_vt::CaptureOpts::default());
        String::from_utf8_lossy(&text)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn keys(&self, server: &mut Server, bytes: &[u8]) {
        server.on_client_input(self.id, bytes).unwrap();
    }
}

/// Provenance (decision 31): a pane that ran in the checkpoint is
/// restored from its write time, one dead there or made with no process
/// is not; a checkpoint of a restored pane still waiting keeps the time
/// it came with; it travels in a live upgrade's snapshot and needs no
/// feature of the reader (SNAPSHOT RULE).
#[test]
fn a_restored_pane_carries_the_checkpoint_time_it_came_from() {
    let (mut server, dir) = restored(
        "provenance",
        &[
            &["split-window", "-d", "-t", "a", "cat", "-"],
            &["split-window", "-d", "-t", "a", ""],
        ],
        &[],
    );
    let _ = &mut server;
    let s = server.checkpoint_snapshot(Mode::Full);
    let from: Vec<_> = s.panes.iter().map(|p| (p.id, p.restored_from)).collect();
    // The empty pane (`split-window ''`) never had a process.
    assert_eq!(from, [(0, Some(WRITTEN)), (1, Some(WRITTEN)), (2, None)]);
    // A waiting pane goes with no dead time, as one that ran: a build
    // before the provenance (3.8.0-beta.2's `rebuild`) skips the dead line
    // of a pane with a dead time and a screen, so it draws this one's.
    let older_reader_draws = |p: &snap::Pane| !(p.dead_time.is_some() && p.term.is_some());
    assert!(s.panes[..2].iter().all(older_reader_draws));
    assert!(s.panes[..2].iter().all(|p| p.exited));
    // A second restart keeps the first checkpoint's time, not its own.
    let (l, socket) = listener(&dir, "s3");
    let mut again = s.clone();
    again.meta.written_us = (WRITTEN + 500) * 1_000_000;
    let again = prepare(
        again,
        &ConfigSays::default(),
        &socket,
        &ServerConfig::default(),
    );
    assert_eq!(again.panes[0].restored_from, Some(WRITTEN));
    let rebuilt = rebuild(l, ServerConfig::default(), &again).unwrap();
    assert_eq!(rebuilt.model.panes[&PaneId(0)].restored_from, Some(WRITTEN));
    assert!(rebuilt.model.panes[&PaneId(0)].dead_time.is_some());
    assert_eq!(rebuilt.waits_restored(PaneId(0)), Some(WRITTEN));
    drop(rebuilt);
    // An upgrade's snapshot carries it, and requires nothing for it.
    let up = server.to_snapshot(snap::Meta {
        purpose: snap::Purpose::Upgrade,
        ..snap::Meta::default()
    });
    assert_eq!(up.panes[0].restored_from, Some(WRITTEN));
    assert!(up.meta.requires.is_empty(), "{:?}", up.meta.requires);
    let decoded = snap::decode(&snap::encode(&up)).unwrap();
    assert_eq!(decoded.panes[0].restored_from, Some(WRITTEN));
    // Killing the pane takes it with it.
    ok(&mut server, &dir, &["kill-pane", "-t", "%1"]);
    assert!(!server.model.panes.contains_key(&PaneId(1)));
    drop(server);
}

/// The hint (decision 31): drawn by the renderer over the restored pane's
/// bottom row, never in its grid (`capture-pane` and a checkpoint don't
/// see it): `Restored <time>: Enter restarts <command>` under `key` and
/// `auto`, `remain-on-exit-format`'s line under `off`, switching with the
/// option.
#[test]
fn the_hint_is_drawn_over_the_bottom_row_not_written_into_the_grid() {
    let (mut server, dir) = restored("hint", &[], &[(0, b"saved\r\nscreen")]);
    let mut screen = Screen::attach(&mut server, &dir);
    let time = server.restored_time(WRITTEN);
    let shown = screen.rows(&mut server);
    assert_eq!(shown[..2], ["saved", "screen"], "{shown:?}");
    assert_eq!(
        shown[23],
        format!("Restored {time}: Enter restarts cat -"),
        "{shown:?}"
    );
    let grid = rows(&mut server, &dir, "%0");
    assert_eq!(grid[..2], ["saved", "screen"]);
    assert!(grid[2..].iter().all(String::is_empty), "{grid:?}");
    // Its checkpoint holds the grid, not the hint: restored again, the
    // pane shows the same rows.
    let s = server.checkpoint_snapshot(Mode::Full);
    let (l, socket) = listener(&dir, "s3");
    let s = prepare(s, &ConfigSays::default(), &socket, &ServerConfig::default());
    let mut again = rebuild(l, ServerConfig::default(), &s).unwrap();
    assert_eq!(rows(&mut again, &dir, "%0"), grid);
    drop(again);

    ok(
        &mut server,
        &dir,
        &["set", "-s", "vtmux-persist-respawn", "off"],
    );
    let shown = screen.rows(&mut server);
    assert!(shown[23].starts_with("Pane is dead (, "), "{shown:?}");
    assert!(
        rows(&mut server, &dir, "%0")
            .iter()
            .all(|r| !r.contains("Pane is dead"))
    );
    ok(
        &mut server,
        &dir,
        &["set", "-s", "vtmux-persist-respawn", "auto"],
    );
    let shown = screen.rows(&mut server);
    assert!(shown[23].starts_with("Restored "), "{shown:?}");
    drop(server);
}

/// Respawning a restored pane (`respawn-pane`, decision 31) keeps its
/// screen: the saved rows stay, a dim `restored <time>` row goes under
/// the last occupied one, the cursor at the start of the next row; the
/// pane is no longer restored. A full screen scrolls into history.
#[test]
fn respawn_keeps_the_screen_above_a_marker_and_the_new_program() {
    let (mut server, dir) = restored(
        "respawn-keep",
        &[&["new-window", "-d", "-t", "a:1", "cat", "-"]],
        &[
            (0, b"one\r\n\x1b[1mtwo\x1b[m\r\n\r\n"),
            (1, b"1\r\n2\r\n3\r\n4\r\n5\r\n6"),
        ],
    );
    let time = server.restored_time(WRITTEN);
    ok(&mut server, &dir, &["respawn-pane", "-t", "%0"]);
    assert_eq!(
        rows(&mut server, &dir, "%0"),
        ["one", "two", &format!("restored {time}"), "", "", ""]
    );
    let pane = &server.model.panes[&PaneId(0)];
    assert_eq!(pane.restored_from, None);
    assert!(!pane.exited && pane.io.is_some());
    assert_eq!(pane.term.cursor(), (0, 3));
    let marker = pane.term.row(2);
    assert!(marker.cells[0].style.faint && !marker.wrapped);
    assert!(pane.term.row(1).cells[0].style.bold, "styles stay");
    // A full screen: the marker scrolls the top row into history.
    ok(&mut server, &dir, &["respawn-pane", "-t", "%1"]);
    assert_eq!(
        rows(&mut server, &dir, "%1"),
        ["3", "4", "5", "6", &format!("restored {time}"), ""]
    );
    assert_eq!(display(&mut server, &dir, "%1", "#{history_size}"), "2");
    // Respawned once, a pane respawns as tmux's: its screen cleared.
    ok(&mut server, &dir, &["respawn-pane", "-k", "-t", "%0"]);
    assert!(rows(&mut server, &dir, "%0").iter().all(String::is_empty));
    drop(server);
}

/// From an alternate screen (an editor): its visible rows are copied
/// below the main screen's last occupied row, then the marker; the main
/// screen is back at the pane's size; after a resize the same holds at
/// the new size; a narrow pane clips the marker.
#[test]
fn respawn_from_an_alternate_screen_keeps_what_it_showed() {
    let (mut server, dir) = restored(
        "respawn-alt",
        &[],
        &[(0, b"shell$ vim\r\n\x1b[?1049h\x1b[H~ edit\r\n~ more")],
    );
    let time = server.restored_time(WRITTEN);
    ok(
        &mut server,
        &dir,
        &["resize-window", "-t", "a", "-x", "30", "-y", "7"],
    );
    ok(&mut server, &dir, &["respawn-pane", "-t", "%0"]);
    let pane = &server.model.panes[&PaneId(0)];
    assert!(!pane.term.alternate_on());
    assert_eq!((pane.term.cols(), pane.term.rows()), (30, 7));
    assert_eq!(
        rows(&mut server, &dir, "%0"),
        [
            "shell$ vim",
            "~ edit",
            "~ more",
            &format!("restored {time}"),
            "",
            "",
            ""
        ]
    );
    drop(server);

    let (mut server, dir) = restored("respawn-narrow", &[], &[(0, b"x")]);
    ok(
        &mut server,
        &dir,
        &["resize-window", "-t", "a", "-x", "8", "-y", "6"],
    );
    ok(&mut server, &dir, &["respawn-pane", "-t", "%0"]);
    let shown = rows(&mut server, &dir, "%0");
    assert_eq!(shown[..2], ["x", "restored"], "{shown:?}");
    assert!(!server.model.panes[&PaneId(0)].term.row(1).wrapped);
    drop(server);
}

/// `respawn-pane -E` keeps the provenance and writes nothing; a respawn
/// whose spawn fails keeps it too, with the screen and no marker; a new
/// command, `-c` and `-e` respawn as any.
#[test]
fn a_failed_or_empty_respawn_keeps_the_pane_restored() {
    let (mut server, dir) = restored("respawn-fail", &[], &[(0, b"kept")]);
    ok(
        &mut server,
        &dir,
        &["respawn-pane", "-E", "-t", "%0", "cat", "-"],
    );
    assert_eq!(server.model.panes[&PaneId(0)].restored_from, Some(WRITTEN));
    assert_eq!(rows(&mut server, &dir, "%0")[..2], ["kept", ""]);
    // A spawn that fails (a word no exec can take).
    let pane = server.model.panes.get_mut(&PaneId(0)).unwrap();
    pane.argv = vec!["ca\0t".to_owned()];
    let failed = server.respawn_restored(vtmux_core::ids::SessionId(0), 0, PaneId(0));
    assert!(failed.is_err_and(|e| e.starts_with("respawn pane failed: ")));
    let pane = &server.model.panes[&PaneId(0)];
    assert_eq!(pane.restored_from, Some(WRITTEN));
    assert!(pane.exited && pane.io.is_none());
    assert_eq!(rows(&mut server, &dir, "%0")[..2], ["kept", ""]);
    let out = run(
        &mut server,
        &dir,
        &[
            "respawn-pane",
            "-t",
            "%0",
            "-e",
            "R=1",
            "sh",
            "-c",
            "exec cat -",
        ],
    );
    assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    assert_eq!(
        display(&mut server, &dir, "%0", "#{pane_start_command}"),
        "sh -c \"exec cat -\""
    );
    assert_eq!(server.model.panes[&PaneId(0)].restored_from, None);
    drop(server);
}

/// The key (decision 32): Enter from a client respawns the active
/// restored pane and goes nowhere else; `send-keys Enter`, `-l`, `-H`
/// and `paste-buffer` do not; a binding on Enter wins; a read-only
/// client's Enter does nothing; `send-keys -K` acts as a client key.
#[test]
fn enter_from_a_client_respawns_and_send_keys_does_not() {
    let (mut server, dir) = restored("enter", &[], &[(0, b"kept")]);
    let mut screen = Screen::attach(&mut server, &dir);
    let _ = screen.rows(&mut server);
    for words in [
        &["send-keys", "-t", "%0", "Enter"][..],
        &["send-keys", "-t", "%0", "-l", "\r"],
        &["send-keys", "-t", "%0", "-H", "0d"],
        &["set-buffer", "\r"],
        &["paste-buffer", "-t", "%0"],
    ] {
        // `paste-buffer` refuses a dead pane, as tmux's does.
        run(&mut server, &dir, words);
        assert_eq!(display(&mut server, &dir, "%0", "#{pane_dead}"), "1");
    }
    ok(
        &mut server,
        &dir,
        &["bind", "-n", "Enter", "set", "-g", "@bound", "1"],
    );
    screen.keys(&mut server, b"\r");
    assert_eq!(
        display(&mut server, &dir, "%0", "#{pane_dead}#{@bound}"),
        "11"
    );
    ok(&mut server, &dir, &["unbind", "-n", "Enter"]);
    server.clients.get_mut(&screen.id).unwrap().flags.read_only = true;
    screen.keys(&mut server, b"\r");
    assert_eq!(display(&mut server, &dir, "%0", "#{pane_dead}"), "1");
    server.clients.get_mut(&screen.id).unwrap().flags.read_only = false;
    screen.keys(&mut server, b"\r");
    assert_eq!(display(&mut server, &dir, "%0", "#{pane_dead}"), "0");
    let shown = screen.rows(&mut server);
    assert!(shown[1].starts_with("restored "), "{shown:?}");
    assert!(
        !shown.iter().any(|r| r.starts_with("Restored ")),
        "{shown:?}"
    );
    drop(server);

    let (mut server, dir) = restored("enter-k", &[], &[]);
    let screen = Screen::attach(&mut server, &dir);
    let _ = screen;
    ok(
        &mut server,
        &dir,
        &["send-keys", "-K", "-c", "/dev/vtmux-test-tty", "Enter"],
    );
    assert_eq!(display(&mut server, &dir, "%0", "#{pane_dead}"), "0");
    drop(server);

    // `off`: Enter is an ordinary key to a dead pane.
    let (mut server, dir) = restored("enter-off", &[], &[]);
    let screen = Screen::attach(&mut server, &dir);
    ok(
        &mut server,
        &dir,
        &["set", "-s", "vtmux-persist-respawn", "off"],
    );
    screen.keys(&mut server, b"\r");
    assert_eq!(display(&mut server, &dir, "%0", "#{pane_dead}"), "1");
    drop(server);
}

/// Synchronized panes (decision 32): Enter respawns the active restored
/// pane only, never a synchronized peer, which stays restored.
#[test]
fn enter_never_respawns_a_synchronized_peer() {
    let (mut server, dir) = restored(
        "enter-sync",
        &[
            &["split-window", "-d", "-t", "a", "cat", "-"],
            &["set", "-w", "-t", "a", "synchronize-panes", "on"],
        ],
        &[],
    );
    let screen = Screen::attach(&mut server, &dir);
    screen.keys(&mut server, b"\r");
    assert_eq!(
        run(
            &mut server,
            &dir,
            &["list-panes", "-t", "a", "-F", "#{pane_id} #{pane_dead}"]
        )
        .lines()
        .skip(1)
        .take(2)
        .collect::<Vec<_>>(),
        ["%0 0", "%1 1"]
    );
    assert_eq!(server.model.panes[&PaneId(1)].restored_from, Some(WRITTEN));
    drop(server);
}

/// `remain-on-exit key` (server-client.c `server_client_handle_dead_key`)
/// does not close a restored pane while the option is `key`; with `off`,
/// and once respawned, it does as in tmux.
#[test]
fn remain_on_exit_key_does_not_close_a_restored_pane_under_key() {
    let setup: &[&[&str]] = &[
        &["split-window", "-d", "-t", "a", "cat", "-"],
        &["set", "-g", "remain-on-exit", "key"],
    ];
    let (mut server, dir) = restored("dead-key", setup, &[]);
    let screen = Screen::attach(&mut server, &dir);
    screen.keys(&mut server, b"x");
    assert!(server.model.panes.contains_key(&PaneId(0)));
    ok(
        &mut server,
        &dir,
        &["set", "-s", "vtmux-persist-respawn", "off"],
    );
    screen.keys(&mut server, b"x");
    assert!(!server.model.panes.contains_key(&PaneId(0)));
    drop(server);
}

/// `auto` (decision 32): once the config has run, every restored pane
/// whose saved command is empty respawns, in its first session; one
/// with a command waits; nothing happens without a cold restore's mark
/// or with `key`.
#[test]
fn auto_respawns_only_restored_shells_after_the_config() {
    let setup: &[&[&str]] = &[
        &["new-window", "-d", "-t", "a:1"],
        &["new-window", "-d", "-t", "a:2"],
        &["new-window", "-d", "-t", "a:3"],
        &["new", "-d", "-s", "b"],
        &["link-window", "-s", "a:1", "-t", "b:5"],
        &["set", "-s", "vtmux-persist-respawn", "auto"],
    ];
    // The config respawns one shell and kills another before the pass.
    let (mut server, dir) = restored_with(
        "auto",
        setup,
        &[(1, b"sh$ ")],
        Some("respawn-pane -t a:2\nkill-pane -t a:3\n"),
    );
    let dead = |server: &mut Server| {
        run(
            server,
            &dir,
            &[
                "list-panes",
                "-a",
                "-F",
                "#{pane_id} #{pane_dead} [#{pane_start_command}]",
            ],
        )
    };
    // The first command client runs the config, then the pass, then its
    // own command.
    let out = dead(&mut server);
    assert_eq!(server.persist.auto_pass, crate::restored::AutoPass::Done);
    assert!(out.contains("%0 1 [cat -]\n"), "{out}");
    for p in ["%1", "%2", "%4"] {
        assert!(out.contains(&format!("{p} 0 []\n")), "{out}");
    }
    assert!(!out.contains("%3 "), "{out}");
    assert_eq!(server.model.panes[&PaneId(0)].restored_from, Some(WRITTEN));
    let time = server.restored_time(WRITTEN);
    let shown = rows(&mut server, &dir, "%1");
    assert_eq!(
        shown[..2],
        ["sh$", &format!("restored {time}")],
        "{shown:?}"
    );
    // The config's respawn was the pane's one: one marker.
    let markers = rows(&mut server, &dir, "%2")
        .iter()
        .filter(|r| r.starts_with("restored "))
        .count();
    assert_eq!(markers, 1);
    drop(server);

    // Under `key` (the default) the pass respawns nothing.
    let setup: &[&[&str]] = &[&["new-window", "-d", "-t", "a:1"]];
    let (mut server, dir) = restored("auto-key", setup, &[]);
    let out = run(
        &mut server,
        &dir,
        &["list-panes", "-a", "-F", "#{pane_id} #{pane_dead}"],
    );
    assert!(out.contains("%1 1\n"), "{out}");
    drop(server);
}

/// `respawn-window` (spawn.c `spawn_window`): the other panes go; the
/// first, restored, respawns keeping its screen at its new size.
#[test]
fn respawn_window_respawns_the_first_pane_as_restored() {
    let (mut server, dir) = restored(
        "respawn-window",
        &[&["split-window", "-v", "-d", "-t", "a", "cat", "-"]],
        &[(0, b"top")],
    );
    let time = server.restored_time(WRITTEN);
    ok(&mut server, &dir, &["respawn-window", "-t", "a:0"]);
    assert!(!server.model.panes.contains_key(&PaneId(1)));
    let shown = rows(&mut server, &dir, "%0");
    assert_eq!(shown.len(), 6, "{shown:?}");
    assert_eq!(shown[..3], ["top", &format!("restored {time}"), ""]);
    drop(server);
}

/// From an alternate screen, what `capture-pane` reads of the copied rows
/// stays (cmd-capture-pane.c `cmd_capture_pane_history`): `-J` joins a
/// soft-wrapped row to the next, also after the pane is widened, and
/// `-e` gives a hyperlink back with its id and URI.
#[test]
fn respawn_from_an_alternate_screen_keeps_its_wraps_and_links() {
    let (mut server, dir) = restored(
        "respawn-alt-wrap",
        &[],
        &[(
            0,
            b"$ vi\r\n\x1b[?1049h\x1b[H\x1b]8;id=k;https://example.com/\x1b\\LINK\x1b]8;;\x1b\\\r\nabcdefghijklmnopqrstABCDEFGHIJKLMNOPQRSTuvwxyz",
        )],
    );
    let joined = |server: &mut Server| {
        run(server, &dir, &["capture-pane", "-p", "-J", "-t", "%0"])
            .lines()
            .skip(1)
            .take(4)
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let wrapped = "abcdefghijklmnopqrstABCDEFGHIJKLMNOPQRSTuvwxyz";
    assert_eq!(joined(&mut server)[..2], ["LINK", wrapped]);
    ok(&mut server, &dir, &["respawn-pane", "-t", "%0"]);
    let out = run(&mut server, &dir, &["capture-pane", "-p", "-e", "-t", "%0"]);
    assert!(
        out.contains("\x1b]8;id=k;https://example.com/\x1b\\LINK"),
        "{out:?}"
    );
    let shown = joined(&mut server);
    assert_eq!(shown[..3], ["$ vi", "LINK", wrapped], "{shown:?}");
    assert!(shown[3].starts_with("restored "), "{shown:?}");
    ok(
        &mut server,
        &dir,
        &["resize-window", "-t", "a", "-x", "50", "-y", "6"],
    );
    let shown = joined(&mut server);
    assert_eq!(shown[..3], ["$ vi", "LINK", wrapped], "{shown:?}");
}

/// `vtmux-persist-respawn off` with an empty `remain-on-exit-format`: the
/// attached client shows the restored pane's saved bottom row, nothing
/// drawn over it, as tmux draws no dead line for an empty format
/// (server-fn.c `server_destroy_pane`).
#[test]
fn an_empty_dead_line_format_draws_nothing_over_the_bottom_row() {
    let (mut server, dir) = restored("hint-empty", &[], &[]);
    let mut screen = Screen::attach(&mut server, &dir);
    let _ = screen.rows(&mut server);
    let pane = server.model.panes.get_mut(&PaneId(0)).unwrap();
    let bottom = pane.term.rows();
    pane.term
        .feed(format!("top\x1b[{bottom};1HBOTTOM-SAVED\x1b[?25l").as_bytes());
    ok(
        &mut server,
        &dir,
        &["set", "-s", "vtmux-persist-respawn", "off"],
    );
    ok(
        &mut server,
        &dir,
        &["set", "-g", "remain-on-exit-format", ""],
    );
    let shown = screen.rows(&mut server);
    assert_eq!(shown[0], "top", "{shown:?}");
    assert_eq!(shown[usize::from(bottom) - 1], "BOTTOM-SAVED", "{shown:?}");
    // A format again, its line over the row.
    ok(
        &mut server,
        &dir,
        &["set", "-g", "remain-on-exit-format", "dead"],
    );
    let shown = screen.rows(&mut server);
    assert_eq!(shown[usize::from(bottom) - 1], "dead", "{shown:?}");
}

/// The hint after a restart that did not run says its status; on a pane
/// too narrow for all of it the time goes first, then the failure, so
/// `Enter restarts` shows; a program that exited 0 (it ran, but was given
/// no input) reads `exited`.
#[test]
fn a_narrow_hint_keeps_its_instruction() {
    let (mut server, dir) = restored("hint-narrow", &[], &[]);
    let mut screen = Screen::attach(&mut server, &dir);
    let time = server.restored_time(WRITTEN);
    let status = |server: &mut Server, n| {
        server.model.panes.get_mut(&PaneId(0)).unwrap().status =
            Some(crate::jobs::JobStatus::Exited(n));
    };
    status(&mut server, 127);
    let shown = screen.rows(&mut server);
    assert_eq!(
        shown[23],
        format!("Restored {time}: restart failed (status 127); Enter restarts cat -")
    );
    for (cols, hint) in [
        ("50", "restart failed (status 127); Enter restarts cat -"),
        ("20", "Enter restarts cat -"),
        ("10", "Enter rest"),
    ] {
        ok(
            &mut server,
            &dir,
            &["resize-window", "-t", "a", "-x", cols, "-y", "24"],
        );
        let shown = screen.rows(&mut server);
        assert!(shown[23].starts_with(hint), "{cols}: {shown:?}");
    }
    status(&mut server, 0);
    ok(
        &mut server,
        &dir,
        &["resize-window", "-t", "a", "-x", "80", "-y", "24"],
    );
    let shown = screen.rows(&mut server);
    assert_eq!(
        shown[23],
        format!("Restored {time}: exited (status 0); Enter restarts cat -")
    );
    drop(server);
}

/// A failed restart's status is this server's: the checkpoint writes the
/// waiting pane with none, and a cold restore of a checkpoint that has one
/// (a build before) drops it, so the next server's hint and
/// `#{pane_dead_status}` say no restart failed.
#[test]
fn a_failed_restarts_status_does_not_survive_a_cold_restore() {
    let (mut server, dir) = restored("status-restore", &[], &[]);
    server.model.panes.get_mut(&PaneId(0)).unwrap().status =
        Some(crate::jobs::JobStatus::Exited(127));
    assert_eq!(
        display(&mut server, &dir, "%0", "#{pane_dead_status}"),
        "127"
    );
    let mut s = server.checkpoint_snapshot(Mode::Full);
    assert!(s.panes[0].status.is_none(), "{:?}", s.panes[0].status);
    s.panes[0].status = Some(snap::ExitStatus {
        code: Some(127),
        ..snap::ExitStatus::default()
    });
    let (l, socket) = listener(&dir, "s3");
    let s = prepare(s, &ConfigSays::default(), &socket, &ServerConfig::default());
    let mut again = rebuild(l, ServerConfig::default(), &s).unwrap();
    assert_eq!(display(&mut again, &dir, "%0", "#{pane_dead_status}"), "");
    assert_eq!(again.model.panes[&PaneId(0)].restored_from, Some(WRITTEN));
    drop(again);
    drop(server);
}
