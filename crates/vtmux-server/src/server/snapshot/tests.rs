//! A rich model built through commands, snapshotted, encoded, decoded and
//! rebuilt into a fresh server: every `list-*` and `show-*` output and a
//! dump of every format variable come out byte-identical (D27 decision
//! 13).

use std::io::Read as _;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use vtmux_core::protocol::{
    ClientFlags, ControlMode, Hello, PROTOCOL_VERSION, ServerMsg, TtyInfo, decode,
};
use vtmux_snapshot as snap;

use super::super::{Client, ClientId, Server, ServerConfig};

/// A temp directory per test thread.
fn temp_dir(tag: &str) -> PathBuf {
    let thread = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
    let dir =
        std::env::temp_dir().join(format!("vtmux-snap-{tag}-{}-{thread}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn listener(dir: &Path, name: &str) -> (std::os::unix::net::UnixListener, PathBuf) {
    let socket = dir.join(name);
    let _ = std::fs::remove_file(&socket);
    (
        std::os::unix::net::UnixListener::bind(&socket).unwrap(),
        socket,
    )
}

fn hello(words: &[&str], dir: &Path, tty: Option<TtyInfo>, control: ControlMode) -> Hello {
    Hello {
        version: PROTOCOL_VERSION,
        invocation: vtmux_core::socket::Invocation::Vtmux,
        pid: 4242,
        argv: words.iter().map(|w| w.as_bytes().to_vec()).collect(),
        cwd: dir.to_path_buf(),
        env: vec![("TERM".into(), "xterm-256color".into())],
        tty,
        flags: ClientFlags {
            control,
            utf8: true,
            no_start: false,
        },
    }
}

/// A client connected through a socket pair, its hello handled; the peer
/// end of its socket.
fn connect(server: &mut Server, h: Hello) -> (ClientId, mio::net::UnixStream) {
    let (ours, peer) = mio::net::UnixStream::pair().unwrap();
    let id = ClientId(server.next_client);
    server.next_client += 1;
    let activity = server.model.touch();
    server.clients.insert(id, Client::new(ours, activity));
    server.on_hello(id, h);
    (id, peer)
}

/// Every message the server has written to `peer`.
fn messages(peer: &mut mio::net::UnixStream) -> Vec<ServerMsg> {
    let mut wire = Vec::new();
    let mut buf = vec![0u8; 65536];
    while let Ok(n @ 1..) = peer.read(&mut buf) {
        wire.extend_from_slice(&buf[..n]);
    }
    let mut out = Vec::new();
    let mut at = 0;
    while at < wire.len() {
        let (msg, len) = decode::<ServerMsg>(&wire[at..]).unwrap();
        out.push(msg);
        at += len;
    }
    out
}

/// Run one command as a command client: its stdout, stderr and exit
/// code, as one text.
fn run(server: &mut Server, dir: &Path, words: &[&str]) -> String {
    let (id, mut peer) = connect(server, hello(words, dir, None, ControlMode::None));
    let pending = std::mem::take(&mut server.pending_exits);
    for (c, code) in pending {
        server.finish_client(c, code);
    }
    let mut out = String::new();
    let mut err = String::new();
    let mut code = None;
    for m in messages(&mut peer) {
        match m {
            ServerMsg::Stdout(b) => out.push_str(&String::from_utf8_lossy(&b)),
            ServerMsg::Stderr(b) => err.push_str(&String::from_utf8_lossy(&b)),
            ServerMsg::Exit { code: c, .. } => code = Some(c),
            _ => {}
        }
    }
    server.drop_client(id);
    format!(
        "$ {}\n{out}--stderr--\n{err}--exit {code:?}--\n",
        words.join(" ")
    )
}

/// Every format variable vtmux knows, as one `name=value` line each.
const VARIABLES: &[&str] = &[
    "session_name",
    "session_id",
    "session_windows",
    "last_window_index",
    "active_window_index",
    "session_stack",
    "session_created",
    "session_activity",
    "session_last_attached",
    "session_marked",
    "session_grouped",
    "session_group",
    "session_group_size",
    "session_group_list",
    "session_group_attached",
    "session_group_many_attached",
    "session_group_attached_list",
    "session_attached",
    "session_active",
    "session_many_attached",
    "session_attached_list",
    "session_alert",
    "session_alerts",
    "session_activity_flag",
    "session_bell_flag",
    "session_silence_flag",
    "session_path",
    "window_id",
    "window_index",
    "window_name",
    "window_activity",
    "window_marked_flag",
    "window_panes",
    "window_width",
    "window_height",
    "window_manual_width",
    "window_manual_height",
    "window_zoomed_flag",
    "window_modal_pane",
    "window_active",
    "window_last_flag",
    "window_start_flag",
    "window_end_flag",
    "window_flags",
    "window_raw_flags",
    "window_linked",
    "window_linked_sessions",
    "window_linked_sessions_list",
    "window_activity_flag",
    "window_bell_flag",
    "window_silence_flag",
    "window_layout",
    "window_visible_layout",
    "window_stack_index",
    "window_active_sessions",
    "window_active_sessions_list",
    "window_active_clients",
    "window_active_clients_list",
    "pane_id",
    "pane_index",
    "pane_pid",
    "pane_width",
    "pane_unzoomed_width",
    "pane_unzoomed_height",
    "pane_height",
    "pane_left",
    "pane_x",
    "pane_top",
    "pane_y",
    "pane_right",
    "pane_bottom",
    "pane_at_left",
    "pane_at_right",
    "pane_at_top",
    "pane_at_bottom",
    "pane_title",
    "pane_fg",
    "pane_bg",
    "pane_active",
    "pane_last",
    "pane_flags",
    "pane_floating_flag",
    "pane_modal_flag",
    "pane_z",
    "pane_zoomed_flag",
    "pane_input_off",
    "pane_synchronized",
    "pane_marked",
    "pane_marked_set",
    "pane_pipe",
    "pane_pipe_pid",
    "pane_unseen_changes",
    "pane_dead",
    "pane_dead_status",
    "pane_dead_signal",
    "pane_dead_time",
    "pane_start_command",
    "pane_start_command_list",
    "pane_start_path",
    "pane_tty",
    "pane_current_command",
    "pane_current_path",
    "cursor_x",
    "cursor_y",
    "cursor_character",
    "alternate_on",
    "synchronized_output_flag",
    "mouse_any_flag",
    "mouse_standard_flag",
    "mouse_button_flag",
    "mouse_all_flag",
    "mouse_sgr_flag",
    "mouse_utf8_flag",
    "pane_key_mode",
    "history_size",
    "history_limit",
    "pane_in_mode",
    "pane_mode",
    "pane_search_string",
    "scroll_position",
    "copy_position",
    "copy_position_limit",
    "copy_line_numbers",
    "copy_cursor_x",
    "copy_cursor_y",
    "top_line_time",
    "copy_cursor_line",
    "copy_cursor_word",
    "selection_mode",
    "rectangle_toggle",
    "selection_active",
    "selection_present",
    "selection_start_x",
    "selection_start_y",
    "selection_end_x",
    "selection_end_y",
    "search_present",
    "search_timed_out",
    "search_count",
    "search_count_partial",
    "search_match",
    "refresh_active",
    "copy_cursor_hyperlink",
    "socket_path",
    "pid",
    "start_time",
    "uid",
    "user",
    "version",
    "config_files",
    "host",
    "host_short",
    "buffer_name",
    "buffer_sample",
    "buffer_full",
    "buffer_size",
    "buffer_created",
    "@user",
    "@pane-opt",
    "status-right",
    "history-limit",
    "mode-keys",
];

const CLIENT_VARIABLES: &[&str] = &[
    "client_name",
    "client_termfeatures",
    "client_colours",
    "client_theme",
    "client_session",
    "client_last_session",
    "client_width",
    "client_height",
    "client_cell_width",
    "client_cell_height",
    "client_termname",
    "client_termtype",
    "client_tty",
    "client_flags",
    "client_control_mode",
    "client_readonly",
    "client_utf8",
    "client_pid",
    "client_key_table",
    "client_prefix",
    "client_discarded",
    "client_written",
    "client_activity",
    "client_created",
    "client_uid",
    "client_user",
];

fn dump(vars: &[&str]) -> String {
    vars.iter()
        .map(|v| format!("{v}=#{{{v}}}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The commands whose output must not change across the snapshot.
fn observations(dir: &Path) -> Vec<Vec<String>> {
    let all = dump(VARIABLES);
    let clients = dump(CLIENT_VARIABLES);
    let mut out: Vec<Vec<String>> = [
        vec!["list-sessions"],
        vec!["list-windows", "-a"],
        vec!["list-panes", "-a"],
        vec!["list-clients"],
        vec!["list-buffers"],
        vec!["list-keys"],
        vec!["list-keys", "-N"],
        vec!["list-keys", "-T", "custom"],
        vec!["show-options", "-g"],
        vec!["show-options", "-s"],
        vec!["show-options", "-gw"],
        vec!["show-options", "-gH"],
        vec!["show-options", "-t", "main"],
        vec!["show-options", "-w", "-t", "main:0"],
        vec!["show-options", "-p", "-t", "%0"],
        vec!["show-options", "-A", "-p", "-t", "%1"],
        vec!["show-hooks", "-g"],
        vec!["show-hooks", "-t", "main"],
        vec!["show-environment", "-g"],
        vec!["show-environment", "-gh"],
        vec!["show-environment", "-t", "main"],
        vec!["show-environment", "-h", "-t", "main"],
        vec!["show-messages"],
        vec!["show-buffer", "-b", "named"],
        vec!["show-prompt-history"],
        vec!["server-access", "-l"],
        vec!["capture-pane", "-p", "-M", "-t", "%0"],
        vec!["capture-pane", "-p", "-e", "-M", "-S", "-", "-t", "%0"],
    ]
    .iter()
    .map(|w| w.iter().map(|s| (*s).to_owned()).collect())
    .collect();
    let _ = dir;
    for f in [
        vec!["list-sessions", "-F", &all],
        vec!["list-windows", "-a", "-F", &all],
        vec!["list-panes", "-a", "-F", &all],
        vec!["list-clients", "-F", &clients],
        vec!["list-clients", "-F", &all],
        vec!["list-buffers", "-F", &all],
        vec!["display-message", "-p", "-t", "main:0.0", &all],
        vec![
            "display-message",
            "-p",
            "#{S:#{session_name}/#{W:#{window_index}#{window_flags}:#{P:#{pane_id}#{?pane_active,*,},} }|}",
        ],
        vec![
            "display-message",
            "-p",
            "#{L:#{client_name} #{client_session},}",
        ],
    ] {
        out.push(f.iter().map(|s| (*s).to_owned()).collect());
    }
    out
}

/// The observations of `server`. The message log's times of day are
/// replaced: the observing commands log themselves when they run, and the
/// two servers run them at different moments (the times of the messages
/// logged before the snapshot are compared with the snapshots).
fn observe(server: &mut Server, dir: &Path) -> String {
    observations(dir)
        .iter()
        .map(|w| {
            let words: Vec<&str> = w.iter().map(String::as_str).collect();
            let out = run(server, dir, &words);
            if words == ["show-messages"] {
                out.lines()
                    .map(|l| match l.as_bytes() {
                        [a, b, b':', c, d, b':', b' ', ..]
                            if [a, b, c, d].iter().all(|x| x.is_ascii_digit()) =>
                        {
                            format!("<TIME>{}\n", &l[5..])
                        }
                        _ => format!("{l}\n"),
                    })
                    .collect()
            } else {
                out
            }
        })
        .collect()
}

/// The snapshot without what legitimately differs between two servers:
/// descriptor numbers and the times left on timers (taken at different
/// instants).
fn normalised(mut s: snap::Snapshot) -> snap::Snapshot {
    s.descriptors.clear();
    for x in &mut s.sessions {
        x.lock_in_ms = x.lock_in_ms.map(|_| 0);
    }
    for c in &mut s.clients {
        c.table_ago_ms = 0;
        if let Some(m) = &mut c.status_message {
            m.timer_in_ms = m.timer_in_ms.map(|_| 0);
        }
        c.repeat_in_ms = c.repeat_in_ms.map(|_| 0);
        if let Some(cs) = &mut c.control {
            cs.subs_due_ms = cs.subs_due_ms.map(|_| 0);
            for q in &mut cs.queued {
                q.age_ms = 0;
            }
        }
    }
    for m in &mut s.server.monitors {
        m.due_in_ms = m.due_in_ms.map(|_| 0);
    }
    for w in &mut s.windows {
        w.silence_in_ms = w.silence_in_ms.map(|_| 0);
    }
    for p in &mut s.panes {
        p.output_ago_ms = p.output_ago_ms.map(|_| 0);
        p.resize_in_ms = p.resize_in_ms.map(|_| 0);
        p.rename_ago_ms = p.rename_ago_ms.map(|_| 0);
        for m in &mut p.modes {
            if let snap::PaneMode::Copy(c) = m {
                c.refresh_in_ms = c.refresh_in_ms.map(|_| 0);
            }
        }
    }
    s
}

/// The bytes fed to each pane's terminal by hand: pane terminal state is
/// not in the snapshot yet (`lu_pane_state`), so the rebuilt server's
/// terminals get the same bytes, standing in for it.
const FED: &[(u32, &[u8])] = &[
    (0, b"hello world\r\nsecond \x1b[1;31mred\x1b[m line\r\n\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\ \xe6\xbc\xa2\r\n"),
    (1, b"\x1b]8;;http://y\x1b\\L\x1b]8;;\x1b\\ink\r\n"),
    (2, b"pane two\r\n"),
];

fn ok(server: &mut Server, dir: &Path, words: &[&str]) {
    let out = run(server, dir, words);
    assert!(
        out.ends_with("--stderr--\n--exit Some(0)--\n"),
        "setup command failed: {out}"
    );
}

/// Build the rich model: sessions (one grouped, one linked window),
/// windows with splits, a zoom, a named layout and a floating pane,
/// options at every level, hooks, keys, aliases, buffers, environment, a
/// wait-for lock, the marked pane, copy and clock modes, an attached
/// terminal client and a control client with a subscription.
#[expect(clippy::too_many_lines, reason = "one command per line")]
fn build(server: &mut Server, dir: &Path) -> Vec<mio::net::UnixStream> {
    for words in [
        &[
            "new-session",
            "-d",
            "-s",
            "main",
            "-n",
            "w0",
            "-x",
            "80",
            "-y",
            "24",
            "cat",
            "-",
        ][..],
        &["set", "-g", "status-right", "x"],
        &["set", "-g", "@user", "user value"],
        &["set", "-g", "command-alias[10]", "ll=list-keys"],
        &["set", "-s", "escape-time", "7"],
        &["set", "-g", "history-limit", "1234"],
        &["set", "-g", "status-bg", "colour21"],
        &["set", "-g", "prefix2", "C-a"],
        &["set", "-g", "default-command", "cat -"],
        &["setw", "-g", "mode-keys", "vi"],
        &["set", "-t", "main", "status-left", "L"],
        &["set", "-t", "main", "status-position", "top"],
        &["set", "-ga", "update-environment", " FOO"],
        &["split-window", "-d", "-h", "-t", "main:0", "cat", "-"],
        &["split-window", "-d", "-v", "-t", "main:0.1", "cat", "-"],
        &["select-layout", "-t", "main:0", "tiled"],
        &["select-pane", "-t", "main:0.1"],
        &["select-pane", "-t", "main:0.2"],
        &["setw", "-t", "main:0", "automatic-rename", "off"],
        &["set", "-p", "-t", "%0", "@pane-opt", "pv"],
        &["set", "-p", "-t", "%1", "pane-border-style", "fg=red"],
        &["new-window", "-d", "-t", "main:3", "-n", "w3", "cat", "-"],
        &["new-window", "-d", "-t", "main:1", "-n", "w1", "cat", "-"],
        &["split-window", "-d", "-t", "main:1", "cat", "-"],
        &["resize-pane", "-Z", "-t", "main:1.0"],
        &[
            "new-pane", "-d", "-t", "main:3", "-x", "20", "-y", "6", "cat", "-",
        ],
        &["new-session", "-d", "-s", "grp", "-t", "main"],
        &["new-session", "-d", "-s", "other", "-n", "o0", "cat", "-"],
        &["link-window", "-s", "other:0", "-t", "main:7"],
        &["rename-window", "-t", "main:3", "renamed"],
        &["select-window", "-t", "main:3"],
        &["select-window", "-t", "main:1"],
        &["select-window", "-t", "main:0"],
        &["select-pane", "-m", "-t", "main:0.2"],
        &["set-hook", "-g", "after-new-window", "display -p hook"],
        &[
            "set-hook",
            "-t",
            "main",
            "after-select-pane",
            "set -g @selected yes",
        ],
        &["bind-key", "-T", "custom", "x", "display", "hi"],
        &["bind-key", "-r", "-N", "a note", "C-y", "select-pane", "-U"],
        &[
            "bind-key",
            "M-q",
            "if",
            "-F",
            "#{pane_active}",
            "{ display yes }",
            "{ display no }",
        ],
        &["unbind-key", "C-z"],
        &["set-buffer", "-b", "named", "named data"],
        &["set-buffer", "auto buffer"],
        &["set-buffer", "second auto"],
        &["set-environment", "-g", "FOO", "bar"],
        &["set-environment", "-gr", "GONE"],
        &["set-environment", "-t", "main", "-h", "HIDDEN", "1"],
        &["set-environment", "-t", "main", "SVAR", "sv"],
        &["wait-for", "-L", "chan"],
        &["wait-for", "-S", "signalled"],
        // Exits when the last end of its socket closes, with both servers.
        &["pipe-pane", "-t", "%2", "exec cat >/dev/null"],
    ] {
        ok(server, dir, words);
    }
    for &(p, bytes) in FED {
        let pane = server
            .model
            .panes
            .get_mut(&vtmux_core::ids::PaneId(p))
            .unwrap();
        pane.term.feed(bytes);
    }
    for words in [
        &["select-pane", "-T", "t0", "-t", "%0"][..],
        &["select-pane", "-T", "t1", "-t", "%1"],
        &["select-pane", "-T", "t2", "-t", "%2"],
        &["copy-mode", "-t", "%0"],
        &["send-keys", "-X", "-t", "%0", "history-top"],
        &["send-keys", "-X", "-t", "%0", "cursor-down"],
        &["send-keys", "-X", "-t", "%0", "begin-selection"],
        &["send-keys", "-X", "-t", "%0", "cursor-right"],
        &["send-keys", "-X", "-t", "%0", "cursor-right"],
        &["send-keys", "-X", "-t", "%0", "search-forward", "red"],
        &["copy-mode", "-t", "%1"],
        &["send-keys", "-X", "-t", "%1", "history-top"],
        &["send-keys", "-X", "-t", "%1", "start-of-line"],
        &["clock-mode", "-t", "%2"],
    ] {
        ok(server, dir, words);
    }
    // An attached terminal client and a control client.
    let tty = TtyInfo {
        name: "/dev/vtmux-test-tty".into(),
        term: "xterm-256color".into(),
        cols: 100,
        rows: 30,
        features: vec!["RGB".into()],
    };
    let (term, term_peer) = connect(
        server,
        hello(
            &["attach-session", "-t", "main"],
            dir,
            Some(tty),
            ControlMode::None,
        ),
    );
    server
        .on_client_input(
            term,
            b"\x1b[?62;22c\x1bP>|xterm(390)\x1b\\\x1b]11;rgb:1010/2020/3030\x1b\\\x1b[?997;2n",
        )
        .unwrap();
    // Read only when a terminal opens (tty-term.c:457, 619): not this
    // client's, before or after the snapshot.
    ok(
        server,
        dir,
        &["set", "-as", "terminal-features", "xterm*:sixel:hyperlinks"],
    );
    ok(
        server,
        dir,
        &[
            "display-message",
            "-c",
            "/dev/vtmux-test-tty",
            "-d",
            "600000",
            "on screen",
        ],
    );
    let (control, peer) = connect(
        server,
        hello(
            &["attach-session", "-t", "grp"],
            dir,
            None,
            ControlMode::Control,
        ),
    );
    server.control_stdin(control, b"refresh-client -B sub::#{session_name}\n");
    server.control_stdin(control, b"refresh-client -f no-output,pause-after=2\n");
    server.control_stdin(control, b"refresh-client -A %1:off\n");
    // Last: the command waits for the popup to close.
    server.control_stdin(
        control,
        b"display-popup -c /dev/vtmux-test-tty -w 30 -h 8 -T pop -b rounded cat -\n",
    );
    server.pending_exits.clear();
    // The clients' ends stay open while the servers live.
    vec![term_peer, peer]
}

fn dup(fd: impl AsFd) -> OwnedFd {
    fd.as_fd().try_clone_to_owned().unwrap()
}

/// The descriptor a live upgrade keeps for `role`, here duplicated from
/// `a`.
fn adopt(a: &Server, role: &snap::Role) -> Option<OwnedFd> {
    match role {
        snap::Role::Client(id) => a
            .clients
            .get(&ClientId(*id as usize))
            .map(|c| dup(&c.stream)),
        snap::Role::PanePty(p) => a
            .model
            .panes
            .get(&vtmux_core::ids::PaneId(*p))
            .and_then(|x| x.io.as_ref())
            .map(|io| dup(io.fd())),
        snap::Role::PipePane(p) => a
            .model
            .panes
            .get(&vtmux_core::ids::PaneId(*p))
            .and_then(|x| x.pipe.as_ref())
            .map(|pipe| dup(pipe.fd())),
        snap::Role::PopupPty(id) => a
            .clients
            .get(&ClientId(*id as usize))
            .and_then(|c| c.popup.as_ref()?.pty())
            .map(|io| dup(io.fd())),
        snap::Role::Listener(()) | snap::Role::Snapshot(()) => None,
    }
}

/// `a` snapshotted, encoded, decoded and rebuilt on a listener named
/// `name`, adopting `a`'s descriptors.
fn rebuild(a: &Server, dir: &Path, name: &str) -> Server {
    let taken = a.to_snapshot(snap::Meta::default());
    let decoded = snap::decode(&snap::encode(&taken)).unwrap();
    decoded.check().unwrap();
    let (l, _) = listener(dir, name);
    Server::from_snapshot(l, ServerConfig::default(), &decoded, &mut |r| adopt(a, r)).unwrap()
}

/// A fresh server on listener `a` in `dir`.
fn server(dir: &Path) -> Server {
    let (l, socket) = listener(dir, "a");
    Server::new(l, socket, ServerConfig::default()).unwrap()
}

/// An attached terminal client of `term` with `features`, on `session`.
fn attach(
    server: &mut Server,
    dir: &Path,
    session: &str,
    features: &[&str],
) -> (ClientId, mio::net::UnixStream) {
    let tty = TtyInfo {
        name: "/dev/vtmux-test-tty".into(),
        term: "xterm-256color".into(),
        cols: 80,
        rows: 24,
        features: features.iter().map(|f| (*f).to_owned()).collect(),
    };
    let out = connect(
        server,
        hello(
            &["attach-session", "-t", session],
            dir,
            Some(tty),
            ControlMode::None,
        ),
    );
    server.pending_exits.clear();
    out
}

#[test]
fn a_rebuilt_model_answers_every_listing_alike() {
    let dir = temp_dir("rich");
    let (l, socket) = listener(&dir, "a");
    let mut a = Server::new(l, socket, ServerConfig::default()).unwrap();
    let _peers = build(&mut a, &dir);

    let meta = snap::Meta {
        purpose: snap::Purpose::Upgrade,
        writer_version: "test".into(),
        ..snap::Meta::default()
    };
    let taken = a.to_snapshot(meta.clone());
    assert!(
        taken
            .descriptors
            .iter()
            .any(|d| matches!(d.role, snap::Role::PanePty(0)))
    );
    let bytes = snap::encode(&taken);
    let decoded = snap::decode(&bytes).unwrap();
    decoded.check().unwrap();
    assert_eq!(decoded, taken);

    assert!(
        taken
            .descriptors
            .iter()
            .any(|d| matches!(d.role, snap::Role::PipePane(2)))
    );
    // The descriptors a live upgrade keeps, here duplicated from `a`.
    let (l, _) = listener(&dir, "b");
    let mut b =
        Server::from_snapshot(l, ServerConfig::default(), &decoded, &mut |r| adopt(&a, r)).unwrap();
    for &(p, bytes) in FED {
        b.model
            .panes
            .get_mut(&vtmux_core::ids::PaneId(p))
            .unwrap()
            .term
            .feed(bytes);
    }

    // Everything the snapshot carries comes back.
    assert_eq!(
        normalised(b.to_snapshot(meta.clone())),
        normalised(a.to_snapshot(meta))
    );
    assert_eq!(b.model.options, a.model.options);
    assert_eq!(b.key_bindings, a.key_bindings);
    let popup = |s: &Server| {
        s.clients
            .values()
            .find_map(|c| c.popup.as_ref())
            .map(|p| (p.title.clone(), p.sx, p.sy, p.pty().is_some()))
    };
    assert_eq!(popup(&a), Some(("pop".to_owned(), 30, 8, true)));
    assert_eq!(popup(&b), popup(&a));

    let before = observe(&mut a, &dir);
    let after = observe(&mut b, &dir);
    for (x, y) in before.split("$ ").zip(after.split("$ ")) {
        assert_eq!(x, y);
    }
    assert_eq!(before, after);
    // The model is rich: nothing failed, and the modes and clients are
    // there to see.
    for want in [
        "copy-mode",
        "clock-mode",
        "/dev/vtmux-test-tty",
        "client-4242",
        "grp",
        "named data",
        "ll=list-keys",
        "custom x",
        "HIDDEN=1",
        "hello world",
        "copy_cursor_hyperlink=http://y",
        "client_termtype=",
    ] {
        assert!(before.contains(want), "{want} missing from {before}");
    }
    let a_pid = a.info.pid;
    assert_eq!(b.info.pid, a_pid);
    drop(b);
    drop(a);
    let _ = std::fs::remove_dir_all(dir);
}

/// Every default key binding and every command option survives the trip
/// through its printed form.
#[test]
fn printed_commands_parse_back_alike() {
    let keys = vtmux_core::keys::KeyBindings::with_defaults();
    for (name, t) in keys.tables() {
        for b in t.bindings() {
            let out = super::binding_out(b);
            let back = super::binding_in(&out).unwrap_or_else(|| panic!("{name} {out:?}"));
            assert_eq!(&back, b, "{name}");
        }
    }
    let store = vtmux_core::options::OptionsStore::new(&vtmux_core::options::Defaults::default());
    let parts = store.export();
    for (_, entries) in &parts.trees {
        assert_eq!(&super::options_in(&super::options_out(entries)), entries);
    }
}

/// A raw descriptor number is what the table records.
#[test]
fn descriptors_name_the_open_sockets_and_ptys() {
    let dir = temp_dir("fds");
    let (l, socket) = listener(&dir, "a");
    let mut a = Server::new(l, socket, ServerConfig::default()).unwrap();
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    let s = a.to_snapshot(snap::Meta::default());
    let pty = a
        .model
        .panes
        .values()
        .next()
        .unwrap()
        .io
        .as_ref()
        .unwrap()
        .fd()
        .as_raw_fd();
    assert!(
        s.descriptors
            .iter()
            .any(|d| d.fd == pty && d.role == snap::Role::PanePty(0))
    );
    assert!(
        s.descriptors
            .iter()
            .any(|d| d.role == snap::Role::Listener(()))
    );
    drop(a);
    let _ = std::fs::remove_dir_all(dir);
}

/// The size of pane `p`'s PTY, as its program reads it (`TIOCGWINSZ`).
fn pty_size(server: &Server, p: u32) -> (u16, u16) {
    let pane = &server.model.panes[&vtmux_core::ids::PaneId(p)];
    let ws = rustix::termios::tcgetwinsize(pane.io.as_ref().unwrap().fd()).unwrap();
    (ws.ws_col, ws.ws_row)
}

/// Size changes the program hasn't been told yet are carried
/// (`wp->resize_queue`), so the rebuilt server still sends them
/// (`server_client_check_pane_resize`, server-client.c:1984-2041).
#[test]
fn queued_pane_sizes_reach_the_program_after_a_rebuild() {
    let dir = temp_dir("resize");
    let mut a = server(&dir);
    ok(
        &mut a,
        &dir,
        &[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "80",
            "-y",
            "24",
            "cat",
            "-",
        ],
    );
    ok(&mut a, &dir, &["split-window", "-h", "-t", "s", "cat", "-"]);
    let queued = |s: &Server| {
        s.model
            .panes
            .values()
            .map(|p| (p.id.0, p.resizes.clone()))
            .collect::<Vec<_>>()
    };
    assert!(
        queued(&a).iter().all(|(_, q)| !q.is_empty()),
        "{:?}",
        queued(&a)
    );
    // The first pane's program still thinks it has the whole window.
    assert_eq!(pty_size(&a, 0), (80, 24));

    let mut b = rebuild(&a, &dir, "b");
    assert_eq!(queued(&b), queued(&a));
    b.send_pane_resizes();
    for p in [0, 1] {
        let rect = b.model.panes[&vtmux_core::ids::PaneId(p)].rect;
        let want = (
            u16::try_from(rect.sx).unwrap(),
            u16::try_from(rect.sy).unwrap(),
        );
        assert_eq!(pty_size(&b, p), want, "%{p}");
    }
    assert_eq!(pty_size(&b, 0), (40, 24));
    drop((a, b));
    let _ = std::fs::remove_dir_all(dir);
}

/// `terminal-features` and `terminal-overrides` are read when a client's
/// terminal opens (`tty_term_create`, tty-term.c:457, 619), never applied
/// to a client already attached: the rebuilt client keeps the features it
/// opened with.
#[test]
fn a_client_keeps_the_terminal_features_it_opened_with() {
    let dir = temp_dir("features");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    let _peer = attach(&mut a, &dir, "s", &[]);
    ok(
        &mut a,
        &dir,
        &["set", "-as", "terminal-features", "xterm*:sixel:hyperlinks"],
    );
    ok(
        &mut a,
        &dir,
        &["set", "-as", "terminal-overrides", "xterm*:Ss=\\E[%p1%d q"],
    );
    let list = ["list-clients", "-F", "#{client_termfeatures}"];
    let before = run(&mut a, &dir, &list);
    assert!(!before.contains("sixel"), "{before}");
    let mut b = rebuild(&a, &dir, "b");
    assert_eq!(run(&mut b, &dir, &list), before);
    let caps = |s: &Server| {
        let c = s.clients.values().find(|c| c.term.is_some()).unwrap();
        c.term.as_ref().unwrap().has_name("Ss")
    };
    assert_eq!(caps(&b), caps(&a));
    drop((a, b));
    let _ = std::fs::remove_dir_all(dir);
}

/// Pane output a `pipe-pane` command has not taken yet stays queued for it
/// (decision 10, keep `pipe-pane` running), and `-I`/`-O` come back as
/// they were.
#[test]
fn pipe_pane_output_not_yet_written_survives() {
    let dir = temp_dir("pipe");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    ok(&mut a, &dir, &["split-window", "-t", "s", "cat", "-"]);
    // Takes nothing until killed, so the socket fills.
    ok(&mut a, &dir, &["pipe-pane", "-t", "%0", "exec sleep 30"]);
    ok(
        &mut a,
        &dir,
        &["pipe-pane", "-I", "-t", "%1", "exec sleep 30"],
    );
    let flood: Vec<u8> = (0..1_000_000u32).map(|i| b'a' + (i % 26) as u8).collect();
    a.pipe_write(vtmux_core::ids::PaneId(0), &flood);
    let pipe = |s: &Server, p: u32| {
        let x = s.model.panes[&vtmux_core::ids::PaneId(p)]
            .pipe
            .as_ref()
            .unwrap();
        (x.pid, x.input(), x.output(), x.pending().to_vec())
    };
    let pending = pipe(&a, 0).3;
    assert!(!pending.is_empty() && flood.ends_with(&pending));
    let taken = a.to_snapshot(snap::Meta::default());
    let outputs: Vec<bool> = taken
        .panes
        .iter()
        .map(|p| p.pipe.as_ref().unwrap().output)
        .collect();
    assert_eq!(outputs, [true, false]);

    let b = rebuild(&a, &dir, "b");
    for p in [0, 1] {
        assert_eq!(pipe(&b, p), pipe(&a, p), "%{p}");
    }
    for p in [0, 1] {
        let pid = rustix::process::Pid::from_raw(pipe(&a, p).0).unwrap();
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
    }
    drop((a, b));
    let _ = std::fs::remove_dir_all(dir);
}

fn from_snapshot(dir: &Path, s: &snap::Snapshot) -> std::io::Result<Server> {
    let (l, _) = listener(dir, "bad");
    Server::from_snapshot(l, ServerConfig::default(), s, &mut |_| None)
}

/// A way to spoil a good snapshot.
type Break = fn(&mut snap::Snapshot);

/// A snapshot that doesn't describe a consistent server is refused with
/// an error, never a panic or a model that hands out ids twice.
#[test]
fn from_snapshot_refuses_inconsistent_input() {
    let dir = temp_dir("bad");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    ok(&mut a, &dir, &["new-window", "-d", "-t", "s", "cat", "-"]);
    let good = a.to_snapshot(snap::Meta::default());
    assert!(from_snapshot(&dir, &good).is_ok());

    let cases: &[(&str, Break)] = &[
        ("dangling link", |s| s.sessions[0].windows[0].window = 42),
        ("window counter", |s| s.server.next_window = 0),
        ("pane counter", |s| s.server.next_pane = 1),
        ("session counter", |s| s.server.next_session = 0),
        ("no active pane", |s| s.windows[0].active = None),
        ("huge pane", |s| {
            s.panes[0].geometry.sx = 1_000_000;
            s.panes[0].geometry.sy = 1_000_000;
        }),
        ("bad link tree", |s| {
            s.sessions[0].link_tree = vec![snap::RbNode { key: 0, red: true }];
        }),
        ("link tree keys", |s| {
            s.sessions[0].link_tree = vec![snap::RbNode { key: 9, red: false }];
        }),
        ("deep link tree", |s| {
            s.sessions[0].link_tree = (0..1_000_000)
                .rev()
                .map(|key| snap::RbNode { key, red: false })
                .collect();
        }),
        ("duplicate name", |s| {
            let mut x = s.sessions[0].clone();
            x.id = 7;
            s.server.next_session = 8;
            s.sessions.push(x);
        }),
    ];
    for (what, f) in cases {
        let mut s = good.clone();
        f(&mut s);
        let got = from_snapshot(&dir, &s);
        assert!(got.is_err(), "{what}: accepted");
    }
    drop(a);
    let _ = std::fs::remove_dir_all(dir);
}

/// Runtime state behind formats and timers is carried: a status-line
/// message and its timer, `lock-after-time`'s time left, when
/// `automatic-rename` last renamed, config errors waiting for a client.
#[test]
fn timers_and_messages_survive_a_rebuild() {
    use std::time::{Duration, Instant};
    let dir = temp_dir("timers");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    ok(&mut a, &dir, &["set", "-g", "lock-after-time", "600"]);
    let (client, _peer) = attach(&mut a, &dir, "s", &[]);
    ok(
        &mut a,
        &dir,
        &[
            "display-message",
            "-c",
            "/dev/vtmux-test-tty",
            "-d",
            "600000",
            "on screen",
        ],
    );
    let s = vtmux_core::ids::SessionId(0);
    let soon = Instant::now() + Duration::from_secs(5);
    a.lock_timers.insert(s, soon);
    let renamed = Instant::now().checked_sub(Duration::from_millis(100));
    a.model
        .panes
        .get_mut(&vtmux_core::ids::PaneId(0))
        .unwrap()
        .last_rename = renamed;
    a.causes = vec!["/x.conf:1: unknown command: nope".into()];

    let b = rebuild(&a, &dir, "b");
    assert_eq!(b.clients[&client].status.message(), Some("on screen"));
    assert!(b.clients[&client].status.is_frozen());
    let left = b.clients[&client].status.deadline().unwrap();
    assert!(
        left <= Instant::now() + Duration::from_secs(600),
        "{left:?}"
    );
    // At most the 5 s it had left, not lock-after-time's 600 s again.
    let lock = b.lock_timers[&s];
    assert!(lock <= Instant::now() + Duration::from_secs(5), "{lock:?}");
    let last = b.model.panes[&vtmux_core::ids::PaneId(0)].last_rename;
    assert!(last.is_some() && last <= Some(Instant::now()));
    assert_eq!(b.causes, a.causes);
    drop((a, b));
    let _ = std::fs::remove_dir_all(dir);
}

/// One monitor hook that can't be parsed doesn't shift the last values and
/// timers of the ones after it onto the wrong monitor.
#[test]
fn an_unreadable_monitor_leaves_the_others_alone() {
    let dir = temp_dir("monitors");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    ok(
        &mut a,
        &dir,
        &["set-hook", "-g", "-B", "@m1:%*:#{pane_id}", "display x"],
    );
    ok(
        &mut a,
        &dir,
        &["set-hook", "-g", "-B", "@m2::#{session_name}", "display y"],
    );
    a.hook_monitors_sync();
    let mut taken = a.to_snapshot(snap::Meta::default());
    assert_eq!(
        taken.server.monitors.len(),
        2,
        "{:?}",
        taken.server.monitors
    );
    let mut bogus = taken.server.monitors[0].clone();
    bogus.what = "no such target".into();
    bogus.serial = 999;
    bogus.due_in_ms = Some(1);
    taken.server.monitors.insert(0, bogus);
    let (l, _) = listener(&dir, "b");
    let b = Server::from_snapshot(l, ServerConfig::default(), &taken, &mut |_| None).unwrap();
    let serials = |s: &Server| -> Vec<(u64, String)> {
        s.hook_monitors
            .parts()
            .into_iter()
            .map(|(serial, p, _)| (serial, p.format))
            .collect()
    };
    assert_eq!(serials(&a).len(), 2);
    assert_eq!(serials(&b), serials(&a));
    drop((a, b));
    let _ = std::fs::remove_dir_all(dir);
}
