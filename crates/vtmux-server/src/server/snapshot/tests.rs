//! A rich model built through commands, snapshotted, encoded, decoded and
//! rebuilt into a fresh server: every `list-*` and `show-*` output and a
//! dump of every format variable come out byte-identical (D27 decision
//! 13).

use std::io::Read as _;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vtmux_core::ids::PaneId;
use vtmux_core::protocol::{
    ClientFlags, ControlMode, Hello, PROTOCOL_VERSION, ServerMsg, TtyInfo, decode,
};
use vtmux_snapshot as snap;

use super::super::{Client, ClientId, Server, ServerConfig};

/// A temp directory per test thread. Under `/tmp`, not `$TMPDIR`: a socket
/// in it must fit `sun_path` (104 bytes on macOS, where `$TMPDIR` is
/// `/var/folders/...`).
pub(crate) fn temp_dir(tag: &str) -> PathBuf {
    let thread = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
    let dir = Path::new("/tmp").join(format!("vtmux-snap-{tag}-{}-{thread}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The test directories do not move with `$TMPDIR`, so a socket in one
/// has a short path whatever it is.
#[test]
fn test_directories_are_under_tmp() {
    let dir = temp_dir("under-tmp");
    assert!(dir.starts_with("/tmp"), "{}", dir.display());
    let (_l, socket) = listener(&dir, "s");
    assert!(socket.as_os_str().len() < 104, "{}", socket.display());
    let _ = std::fs::remove_dir_all(&dir);
}

pub(crate) fn listener(dir: &Path, name: &str) -> (std::os::unix::net::UnixListener, PathBuf) {
    let socket = dir.join(name);
    let _ = std::fs::remove_file(&socket);
    (
        std::os::unix::net::UnixListener::bind(&socket).unwrap(),
        socket,
    )
}

pub(crate) fn hello(
    words: &[&str],
    dir: &Path,
    tty: Option<TtyInfo>,
    control: ControlMode,
) -> Hello {
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
        ext: vtmux_core::protocol::HelloExt::default(),
    }
}

/// A client connected through a socket pair, its hello handled; the peer
/// end of its socket.
pub(crate) fn connect(server: &mut Server, h: Hello) -> (ClientId, mio::net::UnixStream) {
    let (ours, peer) = mio::net::UnixStream::pair().unwrap();
    let id = ClientId(server.next_client);
    server.next_client += 1;
    let activity = server.model.touch();
    server.clients.insert(id, Client::new(ours, activity));
    server.on_hello(id, h);
    (id, peer)
}

/// Every message the server has written to `peer`. A socket pair's buffer
/// is small on macOS, so the server's queue is flushed between reads until
/// both are empty.
pub(crate) fn messages(
    server: &mut Server,
    id: ClientId,
    peer: &mut mio::net::UnixStream,
) -> Vec<ServerMsg> {
    let mut wire = Vec::new();
    let mut buf = vec![0u8; 65536];
    loop {
        let before = wire.len();
        while let Ok(n @ 1..) = peer.read(&mut buf) {
            wire.extend_from_slice(&buf[..n]);
        }
        server.flush_client(id);
        while let Ok(n @ 1..) = peer.read(&mut buf) {
            wire.extend_from_slice(&buf[..n]);
        }
        if wire.len() == before {
            break;
        }
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
pub(crate) fn run(server: &mut Server, dir: &Path, words: &[&str]) -> String {
    let (id, mut peer) = connect(server, hello(words, dir, None, ControlMode::None));
    let pending = std::mem::take(&mut server.pending_exits);
    for (c, code) in pending {
        server.finish_client(c, code);
    }
    let mut out = String::new();
    let mut err = String::new();
    let mut code = None;
    for m in messages(server, id, &mut peer) {
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
    "window_cell_width",
    "window_cell_height",
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
    "pane_output_generation",
    "pane_last_output_time",
    "pane_last_prompt_time",
    "pane_command_start_time",
    "pane_command_end_time",
    "pane_command_running",
    "pane_command_duration",
    "pane_command_status",
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
    "alternate_saved_x",
    "alternate_saved_y",
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
    // Every pane's screen and history as its program left it (copy mode's
    // frozen grid aside): escapes, joined wraps, trailing spaces.
    for p in ["%0", "%1", "%2", "%3", "%6"] {
        for flags in [
            &["-e", "-S", "-"][..],
            &["-J", "-N", "-S", "-"],
            &["-a", "-q"],
        ] {
            let mut w = vec!["capture-pane".to_owned(), "-p".to_owned()];
            w.extend(flags.iter().map(|f| (*f).to_owned()));
            w.extend(["-t".to_owned(), p.to_owned()]);
            out.push(w);
        }
    }
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
        if let Some(q) = &mut c.queries {
            q.start_in_ms = q.start_in_ms.map(|_| 0);
            q.colours_in_ms = q.colours_in_ms.map(|_| 0);
        }
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
        p.scrollbar_hide_in_ms = p.scrollbar_hide_in_ms.map(|_| 0);
        for m in &mut p.modes {
            if let snap::PaneMode::Copy(c) = m {
                c.refresh_in_ms = c.refresh_in_ms.map(|_| 0);
            }
        }
    }
    s
}

/// The bytes fed to each pane's terminal by hand (`cat -` prints only
/// what it is sent): screens with styles, links, wide characters, a
/// wrapped line, history, modes, a scroll region and a cursor away from
/// the end. Only the snapshot brings them to the rebuilt server.
const FED: &[(u32, &[u8])] = &[
    (0, b"hello world\r\nsecond \x1b[1;31mred\x1b[m line\r\n\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\ \xe6\xbc\xa2\r\n"),
    (1, b"\x1b]8;;http://y\x1b\\L\x1b]8;;\x1b\\ink\r\n"),
    (2, b"pane two\r\n\x1b[?1h\x1b[?2004h\x1b[4l\x1b[2;5r\x1b[3;1H\x1b[38;5;200mstyled\x1b[m"),
    (3, b"1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8\r\n9\r\n10\r\n11\r\n12\r\n13\r\n14\r\n15\r\n16\r\n17\r\n18\r\n19\r\n20\r\n21\r\n22\r\n23\r\n24\r\n25\r\n26\r\n27\r\n28\r\n29\r\n30\r\n31\r\n32\r\n33\r\n34\r\n35\r\n36\r\n37\r\n38\r\n39\r\n40\r\n\x1b[?1049h\x1b[5;5Halternate\x1b[6 q"),
    (6, b"a long line that wraps past the edge of a narrow pane\x1b[?25l"),
];

/// Fed to the popup's terminal by hand.
const POPUP_FED: &[u8] = b"in the \x1b[7mpopup\x1b[m\r\n";

pub(crate) fn ok(server: &mut Server, dir: &Path, words: &[&str]) {
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
        // As `Server::pane_input` goes on: whoever pushed kitty flags owns
        // them.
        pane.note_kitty_grants();
        let _ = pane.term.take_events();
    }
    // What OSC 133 said of a finished command (a running one's duration
    // moves with the clock).
    let pane = server
        .model
        .panes
        .get_mut(&vtmux_core::ids::PaneId(6))
        .unwrap();
    pane.output_generation = 5;
    pane.last_output_time = Some(1_790_000_099);
    pane.shell_commands = crate::model::ShellCommands {
        last_prompt: Some(1_790_000_100),
        started: Some(1_790_000_101),
        ended: Some(1_790_000_104),
        running: false,
        status: Some(7),
    };
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
    let popup = server
        .clients
        .values_mut()
        .find_map(|c| c.popup.as_mut())
        .unwrap();
    popup.term.feed(POPUP_FED);
    server.pending_exits.clear();
    // The clients' ends stay open while the servers live.
    vec![term_peer, peer]
}

fn dup(fd: impl AsFd) -> OwnedFd {
    fd.as_fd().try_clone_to_owned().unwrap()
}

/// The descriptor a live upgrade keeps for `role`, here duplicated from
/// `a`.
pub(crate) fn adopt(a: &Server, role: &snap::Role) -> Option<OwnedFd> {
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
pub(crate) fn rebuild(a: &Server, dir: &Path, name: &str) -> Server {
    let taken = a.to_snapshot(snap::Meta::default());
    let decoded = snap::decode(&snap::encode(&taken)).unwrap();
    decoded.check().unwrap();
    let (l, _) = listener(dir, name);
    Server::from_snapshot(l, ServerConfig::default(), &decoded, &mut |r| adopt(a, r)).unwrap()
}

/// A fresh server on listener `a` in `dir`.
pub(crate) fn server(dir: &Path) -> Server {
    let (l, socket) = listener(dir, "a");
    Server::new(l, socket, ServerConfig::default()).unwrap()
}

/// An attached terminal client of `term` with `features`, on `session`.
pub(crate) fn attach(
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

    // Everything the snapshot carries comes back.
    assert_eq!(
        normalised(b.to_snapshot(meta.clone())),
        normalised(a.to_snapshot(meta))
    );
    assert_eq!(b.model.options, a.model.options);
    assert_eq!(b.key_bindings, a.key_bindings);
    let popup = |s: &Server| {
        s.clients.values().find_map(|c| c.popup.as_ref()).map(|p| {
            let opts = vtmux_vt::CaptureOpts {
                escapes: true,
                ..vtmux_vt::CaptureOpts::default()
            };
            let screen = vtmux_vt::capture(&p.term, opts);
            (p.title.clone(), p.sx, p.sy, p.pty().is_some(), screen)
        })
    };
    let shown = popup(&a).unwrap();
    assert_eq!(shown.0, "pop");
    assert_eq!((shown.1, shown.2, shown.3), (30, 8, true));
    assert!(
        shown.4.starts_with(b"in the \x1b[7mpopup\x1b[0m\n"),
        "{shown:?}"
    );
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
        "\x1b[38;5;200mstyled",
        "alternate_on=1",
        "\n40\n",
        "alternate",
        "a long line that wraps past the edge of a narrow pane",
        "copy_cursor_hyperlink=http://y",
        "client_termtype=",
    ] {
        assert!(before.contains(want), "{want} missing from {before}");
    }
    let a_pid = a.info.pid;
    assert_eq!(b.info.pid, a_pid);

    carry_on_alike(&mut a, &mut b, &dir);
    drop(b);
    drop(a);
    let _ = std::fs::remove_dir_all(dir);
}

/// The rebuilt terminals carry on as the old ones do: the scroll region,
/// insert mode, the pending wrap and leaving the alternate screen back to
/// its history.
fn carry_on_alike(old: &mut Server, new: &mut Server, dir: &Path) {
    let mut captures = Vec::new();
    for server in [old, new] {
        for (p, more) in [
            (2, &b"\r\nx\r\ny\r\nz\r\n\x1b[4hAB"[..]),
            (3, b"\x1b[?1049lback\r\n"),
            (6, b"!"),
        ] {
            let pane = server
                .model
                .panes
                .get_mut(&vtmux_core::ids::PaneId(p))
                .unwrap();
            pane.term.feed(more);
        }
        captures.push(
            ["%2", "%3", "%6"]
                .iter()
                .map(|p| {
                    run(
                        server,
                        dir,
                        &["capture-pane", "-p", "-e", "-S", "-", "-t", p],
                    )
                })
                .collect::<String>(),
        );
    }
    assert!(captures[0].contains("\n40\nback\n"), "{}", captures[0]);
    assert_eq!(captures[0], captures[1]);
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

/// What pane `p`'s terminal echoes back (its `cat -` reads in canonical
/// mode with echo on, so a report written to it comes back with ESC as
/// `^[`), until it holds `want` (at most 20 s).
fn echoed(server: &mut Server, p: u32, want: &str) -> String {
    let pane = server
        .model
        .panes
        .get_mut(&vtmux_core::ids::PaneId(p))
        .unwrap();
    let io = pane.io.as_mut().unwrap();
    let end = Instant::now() + Duration::from_secs(20);
    let mut got = String::new();
    while !got.contains(want) {
        match io.read() {
            crate::pane_io::ReadResult::Data(d) => got.push_str(&String::from_utf8_lossy(&d)),
            crate::pane_io::ReadResult::WouldBlock => {
                assert!(Instant::now() < end, "no {want:?} in {got:?}");
                std::thread::sleep(Duration::from_millis(10));
            }
            crate::pane_io::ReadResult::Closed => panic!("closed: {got:?}"),
        }
    }
    got
}

/// D31: a program's grant of mode 2048 is carried, and required
/// ([`snap::INBAND_RESIZE_FEATURE`]: a build that dropped it would leave
/// the program drawing at its old size), and a size queued when the
/// snapshot was taken is reported to it once the rebuilt server sends it
/// (spec D31, *Live upgrade*). Once the program lets the mode go, nothing
/// is required.
#[test]
fn a_mode_2048_grant_and_a_queued_report_survive_a_rebuild() {
    let dir = temp_dir("inband");
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
    ok(&mut a, &dir, &["set", "-s", "vtmux-inband-resize", "on"]);
    a.pane_input(vtmux_core::ids::PaneId(0), b"\x1b[?2048h", false);
    echoed(&mut a, 0, "^[[48;24;80;768;1280t");
    ok(&mut a, &dir, &["split-window", "-h", "-t", "s", "cat", "-"]);
    assert_eq!(pty_size(&a, 0), (80, 24), "the new size is still queued");
    let s = a.to_snapshot(snap::Meta {
        purpose: snap::Purpose::Upgrade,
        ..snap::Meta::default()
    });
    let granted: Vec<bool> = s
        .panes
        .iter()
        .map(|p| p.term.as_ref().unwrap().inband_resize)
        .collect();
    assert_eq!(granted, [true, false]);
    assert_eq!(s.meta.requires, [snap::INBAND_RESIZE_FEATURE]);
    let mut b = rebuild(&a, &dir, "b");
    assert!(
        b.model.panes[&vtmux_core::ids::PaneId(0)]
            .term
            .inband_resize_granted()
    );
    b.send_pane_resizes();
    assert_eq!(pty_size(&b, 0), (40, 24));
    echoed(&mut b, 0, "^[[48;24;40;768;640t");
    b.pane_input(vtmux_core::ids::PaneId(0), b"\x1b[?2048l", false);
    let s = b.to_snapshot(snap::Meta {
        purpose: snap::Purpose::Upgrade,
        ..snap::Meta::default()
    });
    assert!(
        s.panes
            .iter()
            .all(|p| !p.term.as_ref().unwrap().inband_resize)
    );
    assert_eq!(s.meta.requires, Vec::<String>::new());
    drop((a, b));
    let _ = std::fs::remove_dir_all(dir);
}

/// The kernel sends SIGWINCH for a size that differs in pixels alone
/// (`tty_do_resize` compares the whole `winsize`), so a program granted
/// mode 2048 is reported such a size too (a client terminal whose cells
/// changed size, say). (Before: only a change in cells was reported.)
#[test]
fn a_size_that_differs_in_pixels_alone_is_reported_to_a_mode_2048_program() {
    fn pane(a: &mut Server) -> &mut crate::model::Pane {
        a.model.panes.get_mut(&vtmux_core::ids::PaneId(0)).unwrap()
    }
    let dir = temp_dir("inband-pixels");
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
    ok(&mut a, &dir, &["set", "-s", "vtmux-inband-resize", "on"]);
    a.pane_input(vtmux_core::ids::PaneId(0), b"\x1b[?2048h", false);
    echoed(&mut a, 0, "^[[48;24;80;768;1280t");
    // The window is the same size in cells; its cells are half as big.
    pane(&mut a).resizes.push(((80, 24), (80, 24)));
    assert!(
        pane(&mut a).flush_resizes((8, 16)),
        "no report for a pixel change"
    );
    echoed(&mut a, 0, "^[[48;24;80;384;640t");
    // The same again is no change at all.
    pane(&mut a).resizes.push(((80, 24), (80, 24)));
    assert!(!pane(&mut a).flush_resizes((8, 16)));
    drop(a);
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

/// Where an attached client terminal's queries stand comes back as it was:
/// the start timer still due when it was (`tty_start_timer_callback`), so a
/// terminal that never answers DA still gets its redraw; answered queries
/// stay answered.
#[test]
fn a_clients_start_timer_runs_on() {
    let dir = temp_dir("queries");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    let (client, _peer) = attach(&mut a, &dir, "s", &[]);
    let q = a.clients[&client].decoder.queries();
    assert!(q.until.is_some(), "{q:?}");
    let mut q2 = q;
    q2.da2 = true;
    a.clients.get_mut(&client).unwrap().decoder.set_queries(q2);
    let b = rebuild(&a, &dir, "b");
    let back = b.clients[&client].decoder.queries();
    assert!(back.da2 && !back.da, "{back:?}");
    let (was, now) = (q.until.unwrap(), back.until.unwrap());
    assert!(now + Duration::from_secs(1) > was && now <= was + Duration::from_secs(1));
    drop((a, b));
    let _ = std::fs::remove_dir_all(dir);
}

/// A control client stays connected across an upgrade, so one of another
/// protocol version still reads the frozen frames only in the new image,
/// and is still of its version in the next one's snapshot; one of this
/// version reads every frame.
#[test]
fn a_control_client_keeps_its_protocol_version() {
    use vtmux_core::protocol::{FROZEN_SINCE, Speaks};
    assert_ne!(FROZEN_SINCE, PROTOCOL_VERSION);
    let dir = temp_dir("ctlversion");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    let words = ["attach", "-t", "s"];
    let mut older = hello(&words, &dir, None, ControlMode::Control);
    older.version = FROZEN_SINCE;
    let (old_id, _old_peer) = connect(&mut a, older);
    let (new_id, _new_peer) = connect(&mut a, hello(&words, &dir, None, ControlMode::Control));
    assert_eq!(a.clients[&old_id].speaks, Speaks::Frozen);
    let b = rebuild(&a, &dir, "b");
    let c = rebuild(&b, &dir, "c");
    for s in [&b, &c] {
        assert_eq!(s.clients[&old_id].speaks, Speaks::Frozen);
        assert_eq!(s.clients[&new_id].speaks, Speaks::Full);
    }
    drop((a, b, c));
    let _ = std::fs::remove_dir_all(dir);
}

/// A frame the server had read only part of when the snapshot was taken
/// is completed by the bytes after it: the part read goes with the
/// client. Without it the rest decodes as a frame of its own (`frame too
/// large`) and the client is dropped.
#[test]
fn a_frame_read_in_part_is_finished_after_a_rebuild() {
    use std::io::Write as _;
    use vtmux_core::protocol::{ClientMsg, encode};
    let dir = temp_dir("inbuf");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    let (id, mut peer) = connect(
        &mut a,
        hello(&["attach", "-t", "s"], &dir, None, ControlMode::Control),
    );
    let _ = messages(&mut a, id, &mut peer);
    let frame = encode(&ClientMsg::Stdin(b"display-message -p carried\n".to_vec())).unwrap();
    peer.write_all(&frame[..7]).unwrap();
    a.client_readable(id);
    assert_eq!(a.clients[&id].inbuf, frame[..7]);
    let mut b = rebuild(&a, &dir, "b");
    peer.write_all(&frame[7..]).unwrap();
    b.client_readable(id);
    b.cmdq_process();
    let out: Vec<u8> = messages(&mut b, id, &mut peer)
        .into_iter()
        .filter_map(|m| match m {
            ServerMsg::Stdout(bytes) => Some(bytes),
            ServerMsg::Exit { .. } => panic!("the client was dropped: {m:?}"),
            _ => None,
        })
        .flatten()
        .collect();
    let out = String::from_utf8_lossy(&out);
    assert!(out.contains("\ncarried\n%end "), "{out}");
    drop((a, b));
    let _ = std::fs::remove_dir_all(dir);
}

/// The title an attached client's terminal shows from its output comes
/// back with the client.
#[test]
fn a_clients_terminal_title_is_kept() {
    let dir = temp_dir("ttitle");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    let (client, _peer) = attach(&mut a, &dir, "s", &[]);
    let plain = a.to_snapshot(snap::Meta::default());
    assert!(plain.clients.iter().all(|c| c.terminal_title.is_none()));
    a.clients.get_mut(&client).unwrap().terminal_title = Some("INNER".into());
    let b = rebuild(&a, &dir, "b");
    assert_eq!(b.clients[&client].terminal_title.as_deref(), Some("INNER"));
    drop((a, b));
    let _ = std::fs::remove_dir_all(dir);
}

/// A client's view panned by `refresh-client` (`c->pan_window`) comes
/// back with the client.
#[test]
fn a_clients_pan_is_kept() {
    let dir = temp_dir("pan");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    let (client, _peer) = attach(&mut a, &dir, "s", &[]);
    let plain = a.to_snapshot(snap::Meta::default());
    assert!(plain.clients.iter().all(|c| c.pan.is_none()));
    let pan = crate::view::Pan {
        window: vtmux_core::ids::WindowId(0),
        ox: 3,
        oy: 4,
    };
    a.clients.get_mut(&client).unwrap().pan = Some(pan);
    let b = rebuild(&a, &dir, "b");
    assert_eq!(b.clients[&client].pan, Some(pan));
    drop((a, b));
    let _ = std::fs::remove_dir_all(dir);
}

/// D-014: a window's cell size in pixels, and the one waiting with a
/// pending size, come back as they were; the panes' replies use it.
#[test]
fn a_windows_cell_pixels_are_kept() {
    let dir = temp_dir("cellpx");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    let w = vtmux_core::ids::WindowId(0);
    let window = a.model.windows.get_mut(&w).unwrap();
    window.cell_pixels = (10, 20);
    window.pending_size = Some((70, 20));
    window.pending_pixels = (8, 16);
    let b = rebuild(&a, &dir, "b");
    let window = &b.model.windows[&w];
    assert_eq!(
        (window.cell_pixels, window.pending_pixels),
        ((10, 20), (8, 16))
    );
    assert_eq!(b.model.panes[&PaneId(0)].term.cell_pixels(), (10, 20));
    // A snapshot from a build before has none: tmux's default.
    let c = rebuild_with(&a, &dir, |s| s.windows[0].cell_pixels = (0, 0));
    assert_eq!(c.model.windows[&w].cell_pixels, (16, 32));
    drop((a, b, c));
    let _ = std::fs::remove_dir_all(dir);
}

/// A client's view comes back as it was, not worked out again: tmux works
/// it out only on some events, so it may lag what it follows.
#[test]
fn a_clients_view_is_kept() {
    let dir = temp_dir("view");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    let (client, _peer) = attach(&mut a, &dir, "s", &[]);
    let view = crate::view::View {
        ox: 1,
        oy: 2,
        sx: 3,
        sy: 4,
        bigger: true,
    };
    a.clients.get_mut(&client).unwrap().view = view;
    let b = rebuild(&a, &dir, "b");
    assert_eq!(b.clients[&client].view, view);
    drop((a, b));
    let _ = std::fs::remove_dir_all(dir);
}

/// D33: a kitty graphics grant survives a live upgrade, required while its
/// program runs, with its owner; the images do not (the snapshot is
/// image-free): the program's next command is answered, its placements are
/// gone. A grant whose process group is gone ends on import. A checkpoint
/// drops the grant: a restored pane is dead.
#[test]
fn a_graphics_grant_survives_a_rebuild_and_its_images_do_not() {
    use crate::pane_kitty::fake;
    let dir = temp_dir("graphics");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    ok(&mut a, &dir, &["split-window", "-t", "s", "cat", "-"]);
    ok(&mut a, &dir, &["set", "-s", "vtmux-graphics", "on"]);
    let (p0, p1) = (vtmux_core::ids::PaneId(0), vtmux_core::ids::PaneId(1));
    let (alive, dead) = (fake::FAKE_BASE + 21, fake::FAKE_BASE + 22);
    fake::set_foreground(p0, alive);
    fake::set_foreground(p1, dead);
    let image = b"\x1b_Ga=T,f=24,s=1,v=1,i=1,q=2;/wAA\x1b\\";
    a.pane_input(p0, image, false);
    a.pane_input(p1, image, false);
    let s = a.to_snapshot(snap::Meta::default());
    assert_eq!(s.meta.requires, [snap::GRAPHICS_GRANT_FEATURE]);
    assert_eq!(s.panes[0].graphics_owner_primary, Some(alive));
    assert!(s.panes[0].term.as_ref().unwrap().graphics_primary);
    fake::kill(dead);
    let mut b = rebuild(&a, &dir, "b");
    let pane = &b.model.panes[&p0];
    assert_eq!(pane.term.graphics_grants(), [true, false]);
    assert_eq!(pane.graphics.owner, [Some(alive), None]);
    assert_eq!(pane.term.graphics_storage(false).0, 0, "no images");
    assert_eq!(pane.term.graphics_placements(), []);
    assert_eq!(
        b.model.panes[&p1].term.graphics_grants(),
        [false; 2],
        "its group is gone"
    );
    assert!(b.model.graphics_used);
    // Still granted with the option off: the next image loads.
    ok(&mut b, &dir, &["set", "-s", "vtmux-graphics", "off"]);
    b.pane_input(p0, image, false);
    assert_eq!(b.model.panes[&p0].term.graphics_storage(false).0, 3);
    // A checkpoint keeps no grant.
    let saved = b.checkpoint_snapshot(crate::server::persist::Mode::Full);
    assert!(saved.panes.iter().all(|p| {
        let t = p.term.as_ref().unwrap();
        !t.graphics_primary && !t.graphics_alternate && p.graphics_owner_primary.is_none()
    }));
    assert!(
        !saved
            .meta
            .requires
            .contains(&snap::GRAPHICS_GRANT_FEATURE.to_string())
    );
    fake::clear();
    drop((a, b));
    let _ = std::fs::remove_dir_all(dir);
}

/// A downgrade to the build before works with an ordinary attached client
/// (decision 6 refuses a newer format, not a feature a fix adds): nothing
/// such a client produces, its terminal's answered queries and a title set
/// on it, is in `Meta::requires`, which an older binary refuses when it
/// names something it lacks. The build before this rule (54ad112) knew no
/// feature names at all. Only state an ordinary session doesn't produce
/// may be required: a shown auto-hide scrollbar, or history in Ghostty's
/// form onto a binary of the same patch stack.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one session with every ordinary piece of state set"
)]
fn an_ordinary_attached_client_requires_no_feature() {
    let dir = temp_dir("ordinary");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["set", "-g", "set-titles", "on"]);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    let (client, _peer) = attach(&mut a, &dir, "s", &[]);
    a.on_client_input(
        client,
        b"\x1b[?62;22c\x1b[>84;0;0c\x1bP>|xterm(390)\x1b\\\x1b]11;rgb:1010/2020/3030\x1b\\",
    )
    .unwrap();
    // A program's progress bar, shown and then hidden with its progress
    // left over, is state an ordinary session holds.
    a.pane_input(vtmux_core::ids::PaneId(0), b"\x1b]9;4;3;\x1b\\", false);
    a.pane_input(vtmux_core::ids::PaneId(0), b"\x1b]9;4;1;50\x1b\\", false);
    let shown = a.to_snapshot(snap::Meta::default());
    assert_eq!(
        shown.panes[0].term.as_ref().unwrap().progress_bar.progress,
        50
    );
    a.pane_input(vtmux_core::ids::PaneId(0), b"\x1b]9;4;0\x1b\\", false);
    // A program that asks for in-band resize (D31) is ordinary too, and
    // so is the option turned on: a set while it was off is no grant
    // (a grant is required,
    // `a_mode_2048_grant_and_a_queued_report_survive_a_rebuild`).
    a.pane_input(vtmux_core::ids::PaneId(0), b"\x1b[?2048h", false);
    ok(&mut a, &dir, &["set", "-s", "vtmux-inband-resize", "on"]);
    a.pane_input(vtmux_core::ids::PaneId(0), b"\x1b[?2048$p", false);
    // A link the client was drawn (OSC 8, `drawn_links`) is too.
    a.pane_input(
        vtmux_core::ids::PaneId(0),
        b"\x1b]8;;http://x\x1b\\L\x1b]8;;\x1b\\",
        false,
    );
    // Its program's output (`#{pane_last_output_time}`) is ordinary too.
    a.model
        .panes
        .get_mut(&vtmux_core::ids::PaneId(0))
        .unwrap()
        .last_output_time = Some(1_790_000_099);
    // Kitty graphics (D33): a command while `vtmux-graphics` is off is a
    // title, and the option turned on grants nothing before the next one
    // (a grant is required,
    // `a_graphics_grant_survives_a_rebuild_and_its_images_do_not`).
    a.pane_input(
        vtmux_core::ids::PaneId(0),
        b"\x1b_Ga=q,i=1,s=1,v=1,f=24;AAAA\x1b\\",
        false,
    );
    ok(&mut a, &dir, &["set", "-s", "vtmux-graphics", "on"]);
    // A kitty keyboard grant (D29) with the process group that pushed it.
    ok(&mut a, &dir, &["set", "-s", "vtmux-kitty-keys", "on"]);
    a.pane_input(vtmux_core::ids::PaneId(0), b"\x1b[>1u", false);
    assert!(
        a.to_snapshot(snap::Meta::default()).panes[0]
            .kitty_owner_primary
            .is_some()
    );
    a.render_clients();
    let drawn = a.to_snapshot(snap::Meta::default());
    assert_eq!(drawn.panes[0].term.as_ref().unwrap().drawn_links.len(), 1);
    // And the counter that numbers them (`Server::link_number`).
    let kept = drawn.panes[0].term.as_ref().unwrap().drawn_links[0].number;
    assert!(drawn.server.link_number > kept);
    // As a redraw does (server_client_set_title).
    let session = a.clients[&client].attached.unwrap();
    assert!(a.client_title(client, session).is_some());
    // A window whose pane started in a directory not named in UTF-8
    // (`cwd_bytes`).
    if let Some(bad) = non_utf8_dir(&dir) {
        ok(&mut a, &bad, &["new-window", "-d", "-t", "s", "cat", "-"]);
    }
    // A session made by an attaching client keeps its terminal's control
    // characters for its panes (`s->tio`).
    a.model
        .sessions
        .get_mut(&vtmux_core::ids::SessionId(0))
        .unwrap()
        .control_chars = Some(vec![3, 28, 127, 21]);
    // A window sized by a client whose terminal gave its cell size in
    // pixels (D-014).
    a.clients.get_mut(&client).unwrap().cell_pixels = (10, 20);
    ok(&mut a, &dir, &["set", "-g", "status", "off"]);
    a.check_window_resizes();
    assert_eq!(
        a.model.windows[&vtmux_core::ids::WindowId(0)].cell_pixels,
        (10, 20)
    );
    // An editor up over history: the primary history it hides is read in
    // place (Ghostty patch 0038), and only the visible rows are kept. The
    // rows form also carries the copy the build before shows there.
    a.pane_input(vtmux_core::ids::PaneId(0), &b"line\r\n".repeat(200), false);
    a.pane_input(vtmux_core::ids::PaneId(0), b"\x1b[?1049hvim", false);
    // Keys its program has not read yet (`input_queued`).
    a.model
        .panes
        .get_mut(&PaneId(0))
        .unwrap()
        .io
        .as_mut()
        .unwrap()
        .queue(b"typed ahead");
    // A frame from the client read in part (`inbuf`).
    a.clients.get_mut(&client).unwrap().inbuf = vec![0x03, 0x00];
    // A client stopped by `suspend-client` (`stopped`).
    a.suspend_client(client);
    let s = a.to_snapshot(snap::Meta {
        purpose: snap::Purpose::Upgrade,
        ..snap::Meta::default()
    });
    assert_eq!(s.panes[0].input_queued, b"typed ahead");
    assert_eq!(s.clients[0].inbuf, [0x03, 0x00]);
    assert!(s.clients[0].stopped);
    let [c] = &s.clients[..] else {
        panic!("{:?}", s.clients)
    };
    let q = c.queries.as_ref().expect("its queries are carried");
    assert!(q.da && q.da2 && q.xtversion, "{q:?}");
    assert!(c.terminal_title.is_some(), "set-titles set one");
    assert!(!s.panes[0].term.as_ref().unwrap().inband_resize);
    let alt = s.panes[0].term.as_ref().unwrap().alt_entry.as_ref();
    assert!(alt.is_some_and(|alt| alt.has_saved && alt.history.is_empty()));
    assert_eq!(s.sessions[0].control_chars, Some(vec![3, 28, 127, 21]));
    assert_eq!(s.meta.requires, Vec::<String>::new());
    let rows = a.to_snapshot_with(
        snap::Meta {
            purpose: snap::Purpose::Upgrade,
            ..snap::Meta::default()
        },
        Some(crate::snapshot_term::Form::Rows),
    );
    let term = rows.panes[0].term.as_ref().unwrap();
    let alt = term.alt_entry.as_ref().expect("the editor's entry");
    let copied: u64 = alt.history.iter().map(|p| u64::from(p.rows)).sum();
    assert!(
        alt.has_history && copied == term.history.hsize && copied > 150,
        "{copied}"
    );
    assert_eq!(rows.meta.requires, Vec::<String>::new());
    // A row lacking a line flag its cells say (`snap::Row::lacks`): a
    // scroll in an RGB background with no history, in a second pane (the
    // first keeps its history for the editor above).
    ok(&mut a, &dir, &["set", "-g", "history-limit", "0"]);
    ok(&mut a, &dir, &["split-window", "-d", "-t", "s", "cat", "-"]);
    let second = *a.model.panes.keys().max().unwrap();
    a.pane_input(second, b"\x1b[48;2;9;8;7m\x1b[S\x1b[m", false);
    let scrolled = a.to_snapshot(snap::Meta::default());
    let rows = &scrolled
        .panes
        .last()
        .unwrap()
        .term
        .as_ref()
        .unwrap()
        .primary
        .rows;
    assert!(rows.iter().any(|r| r.lacks.extended));
    assert_eq!(scrolled.meta.requires, Vec::<String>::new());
    let rebuilt = rebuild(&a, &dir, "b");
    assert_eq!(
        rebuilt.model.sessions[&vtmux_core::ids::SessionId(0)].control_chars,
        Some(vec![3, 28, 127, 21])
    );
    drop((a, rebuilt));
    let _ = std::fs::remove_dir_all(dir);
}

/// A server with session `s` (pane %0, `cat -`) whose option
/// `vtmux-kitty-keys` is on, and the pane's foreground process group as
/// `foreground` says (a fake one, alive until killed).
fn kitty_server(dir: &Path, foreground: i32) -> Server {
    let mut a = server(dir);
    ok(&mut a, dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    ok(&mut a, dir, &["set", "-s", "vtmux-kitty-keys", "on"]);
    crate::pane_kitty::fake::set_foreground(PaneId(0), foreground);
    a
}

/// What a key sent to %0 is encoded with, and its stack as exported.
fn kitty_of(server: &mut Server) -> (u8, [u8; 8]) {
    let kitty = server.model.key_modes_now(PaneId(0)).unwrap().kitty;
    let state = server.model.panes[&PaneId(0)].term.export_state();
    (kitty, state.primary.kitty_flags)
}

/// `a` rebuilt from its snapshot as `edit` leaves it.
fn rebuild_with(a: &Server, dir: &Path, edit: impl FnOnce(&mut snap::Snapshot)) -> Server {
    let mut taken = a.to_snapshot(snap::Meta::default());
    edit(&mut taken);
    let decoded = snap::decode(&snap::encode(&taken)).unwrap();
    decoded.check().unwrap();
    let (l, _) = listener(dir, "b");
    Server::from_snapshot(l, ServerConfig::default(), &decoded, &mut |r| adopt(a, r)).unwrap()
}

/// D29: the grant ends with the process group that pushed it. A program
/// killed without popping its flags leaves the shell encoded as kitty
/// (C-c as `CSI 99;5u`) until a `reset`; the group in the PTY's foreground
/// when the flags went non-zero is remembered, and the stack is emptied
/// once it has no process, before the next key is encoded and the next
/// output parsed. A running program keeps its grant, whatever the option
/// says (D28 rule 5), and so does one a pop emptied then granted again.
#[test]
fn a_grant_ends_with_the_process_group_that_pushed_it() {
    use crate::pane_kitty::fake;
    let dir = temp_dir("kittyowner");
    let (a_group, b_group) = (fake::FAKE_BASE + 1, fake::FAKE_BASE + 2);
    let mut a = kitty_server(&dir, a_group);
    let p = PaneId(0);
    a.pane_input(p, b"\x1b[>1u", false);
    assert_eq!(a.model.panes[&p].kitty_owner, [Some(a_group), None]);
    ok(&mut a, &dir, &["set", "-s", "vtmux-kitty-keys", "off"]);
    assert_eq!(kitty_of(&mut a).0, 1, "alive");
    // Popped: no owner, and the next program's push is its own.
    a.pane_input(p, b"\x1b[<u", false);
    assert_eq!(a.model.panes[&p].kitty_owner, [None; 2]);
    ok(&mut a, &dir, &["set", "-s", "vtmux-kitty-keys", "on"]);
    fake::set_foreground(p, b_group);
    a.pane_input(p, b"\x1b[>1u", false);
    assert_eq!(a.model.panes[&p].kitty_owner, [Some(b_group), None]);
    // Killed: the stack is gone before the key is encoded.
    fake::kill(b_group);
    assert_eq!(kitty_of(&mut a).0, 0);
    assert_eq!(a.model.panes[&p].kitty_owner, [None; 2]);
    assert_eq!(kitty_of(&mut a).1, [0; 8]);
    // Output after a kill sees no stack: a push while the option is off
    // is dropped, which a stale grant would let through.
    ok(&mut a, &dir, &["set", "-s", "vtmux-kitty-keys", "on"]);
    fake::set_foreground(p, fake::FAKE_BASE + 3);
    a.pane_input(p, b"\x1b[>1u", false);
    fake::kill(fake::FAKE_BASE + 3);
    ok(&mut a, &dir, &["set", "-s", "vtmux-kitty-keys", "off"]);
    a.pane_input(p, b"\x1b[>8u", false);
    assert_eq!(a.model.panes[&p].term.kitty_keys(), 0, "no stale push");
    fake::clear();
    drop(a);
    let _ = std::fs::remove_dir_all(dir);
}

/// The owner of each screen is its own: a program on the alternate screen
/// that is killed ends only that screen's grant, whichever screen is in
/// use, and the shell's keeps going.
#[test]
fn the_alternate_screens_grant_ends_alone() {
    use crate::pane_kitty::fake;
    let dir = temp_dir("kittyalt");
    let (shell, editor) = (fake::FAKE_BASE + 1, fake::FAKE_BASE + 2);
    let mut a = kitty_server(&dir, shell);
    let p = PaneId(0);
    a.pane_input(p, b"\x1b[>1u", false);
    fake::set_foreground(p, editor);
    a.pane_input(p, b"\x1b[?1049h\x1b[>4u", false);
    assert_eq!(a.model.panes[&p].kitty_owner, [Some(shell), Some(editor)]);
    fake::kill(editor);
    // Still in the alternate screen: its stack goes, the shell's stays.
    assert_eq!(kitty_of(&mut a).0, 0);
    assert_eq!(a.model.panes[&p].kitty_owner, [Some(shell), None]);
    a.pane_input(p, b"\x1b[?1049l", false);
    assert_eq!(kitty_of(&mut a).0, 1, "the shell's own");
    // The same with the screen not in use when the program ends.
    fake::set_foreground(p, fake::FAKE_BASE + 3);
    a.pane_input(p, b"\x1b[?1049h\x1b[>4u\x1b[?1049l", false);
    assert_eq!(a.model.panes[&p].kitty_owner[1], Some(fake::FAKE_BASE + 3));
    fake::kill(fake::FAKE_BASE + 3);
    assert_eq!(kitty_of(&mut a).0, 1);
    a.pane_input(p, b"\x1b[?1049h", false);
    assert_eq!(a.model.panes[&p].term.kitty_keys(), 0, "no stale stack");
    fake::clear();
    drop(a);
    let _ = std::fs::remove_dir_all(dir);
}

/// D29 across a live upgrade: the owner is in the snapshot, so a running
/// program keeps its grant (rule 5) and one gone meanwhile loses it on
/// import.
#[test]
fn a_grant_and_its_owner_cross_a_rebuild() {
    use crate::pane_kitty::fake;
    let dir = temp_dir("kittyresume");
    let group = fake::FAKE_BASE + 1;
    let mut a = kitty_server(&dir, group);
    a.pane_input(PaneId(0), b"\x1b[>1u", false);
    let mut b = rebuild_with(&a, &dir, |_| {});
    assert_eq!(b.model.panes[&PaneId(0)].kitty_owner, [Some(group), None]);
    assert_eq!(kitty_of(&mut b).0, 1);
    drop(b);
    fake::kill(group);
    let mut b = rebuild_with(&a, &dir, |_| {});
    assert_eq!(kitty_of(&mut b), (0, [0; 8]), "gone by the import");
    assert_eq!(b.model.panes[&PaneId(0)].kitty_owner, [None; 2]);
    fake::clear();
    drop((a, b));
    let _ = std::fs::remove_dir_all(dir);
}

/// A snapshot that names no owner (a build before it, or a downgrade and
/// back, where the program may have ended unseen) is settled on import:
/// the foreground group is taken as the pusher, whether a job's or the
/// pane's own process (an exec'd program; rule 5, never drop a running
/// program's grant); the primary screen not in use is the shell's, the
/// alternate one's is gone.
#[test]
fn a_grant_with_no_recorded_owner_is_settled_on_import() {
    use crate::pane_kitty::fake;
    let dir = temp_dir("kittysettle");
    let program = fake::FAKE_BASE + 1;
    let mut a = kitty_server(&dir, program);
    let p = PaneId(0);
    let shell = a.model.panes[&p].pid;
    a.pane_input(p, b"\x1b[>1u", false);
    let unnamed = |s: &mut snap::Snapshot| {
        s.panes[0].kitty_owner_primary = None;
        s.panes[0].kitty_owner_alternate = None;
    };
    // A program in the foreground: its grant stays, and is its own now.
    let mut b = rebuild_with(&a, &dir, unnamed);
    assert_eq!(kitty_of(&mut b).0, 1);
    assert_eq!(b.model.panes[&p].kitty_owner, [Some(program), None]);
    fake::kill(program);
    assert_eq!(kitty_of(&mut b).0, 0, "and ends with it");
    drop(b);
    // The pane's own process group in the foreground: a program that
    // was exec'd as the pane, which lives as long as the pane (rule 5);
    // see `a_grant_in_the_panes_own_process_group_survives_an_import`.
    fake::clear();
    fake::set_foreground(p, shell);
    let mut b = rebuild_with(&a, &dir, unnamed);
    assert_eq!(kitty_of(&mut b).0, 1);
    assert_eq!(b.model.panes[&p].kitty_owner, [Some(shell), None]);
    drop(b);
    // The alternate screen's program in front, the shell's grant behind.
    fake::set_foreground(p, program);
    a.pane_input(p, b"\x1b[?1049h\x1b[>4u", false);
    let mut b = rebuild_with(&a, &dir, unnamed);
    assert_eq!(kitty_of(&mut b).0, 4);
    assert_eq!(b.model.panes[&p].kitty_owner, [None, Some(program)]);
    b.pane_input(p, b"\x1b[?1049l", false);
    assert_eq!(kitty_of(&mut b).0, 1, "the shell's");
    drop(b);
    // The alternate screen left with its stack: gone. The shell's stays.
    a.pane_input(p, b"\x1b[?1049l", false);
    let mut b = rebuild_with(&a, &dir, unnamed);
    assert_eq!(kitty_of(&mut b).0, 1);
    b.pane_input(p, b"\x1b[?1049h", false);
    assert_eq!(b.model.panes[&p].term.kitty_keys(), 0, "its stack is gone");
    fake::clear();
    drop((a, b));
    let _ = std::fs::remove_dir_all(dir);
}

/// A program that is the pane's own process (`new-session cat -`, or mu's
/// `exec /bin/sh -c ... exec "$SHELL" -c "$c"`) has the PTY's foreground
/// group for its whole life: it is the pane's process group. An import of
/// a snapshot that names no owner (from a build before the owners, in
/// either direction: main -> this, this -> main -> this) took it for "the
/// shell, the pusher gone" and dropped a running program's grant (D28
/// rule 5). Real processes and the real foreground group, no fakes.
#[test]
fn a_grant_in_the_panes_own_process_group_survives_an_import() {
    let unnamed = |s: &mut snap::Snapshot| {
        s.panes[0].kitty_owner_primary = None;
        s.panes[0].kitty_owner_alternate = None;
    };
    let wrapper = r#"c='exec cat -'; exec "${SHELL:-/bin/sh}" -c "$c""#;
    let launches: [(&str, &[&str]); 2] = [
        ("direct", &["cat", "-"]),
        ("mu wrapper", &["/bin/sh", "-c", wrapper]),
    ];
    for (shape, command) in launches {
        let dir = temp_dir("kittyown");
        let mut first = server(&dir);
        let mut new = vec!["new-session", "-d", "-s", "s"];
        new.extend(command);
        ok(&mut first, &dir, &new);
        ok(&mut first, &dir, &["set", "-s", "vtmux-kitty-keys", "on"]);
        let p = PaneId(0);
        let pid = first.model.panes[&p].pid;
        // The pane's tty has its session by the time the child runs.
        let end = Instant::now() + Duration::from_secs(10);
        while first.model.panes[&p].foreground_pgid() != Some(pid) {
            assert!(Instant::now() < end, "{shape}: foreground never the pane's");
            std::thread::sleep(Duration::from_millis(10));
        }
        first.pane_input(p, b"\x1b[>1u", false);
        assert_eq!(
            first.model.panes[&p].kitty_owner,
            [Some(pid), None],
            "{shape}"
        );
        // this -> this: the owner is in the snapshot.
        let mut same = rebuild_with(&first, &dir, |_| {});
        assert_eq!(kitty_of(&mut same).0, 1, "{shape}: this -> this");
        drop(same);
        // main -> this: no owner recorded.
        let mut main_this = rebuild_with(&first, &dir, unnamed);
        assert_eq!(kitty_of(&mut main_this).0, 1, "{shape}: main -> this");
        assert_eq!(
            main_this.model.panes[&p].kitty_owner,
            [Some(pid), None],
            "{shape}"
        );
        // this -> main -> this: and once more through another build.
        let mut twice = rebuild_with(&main_this, &dir, unnamed);
        assert_eq!(kitty_of(&mut twice).0, 1, "{shape}: this -> main -> this");
        // The alternate screen's grant is the same pane process's.
        twice.pane_input(p, b"\x1b[?1049h\x1b[>4u", false);
        let mut alt = rebuild_with(&twice, &dir, unnamed);
        assert_eq!(kitty_of(&mut alt).0, 4, "{shape}: alternate");
        assert_eq!(
            alt.model.panes[&p].kitty_owner,
            [None, Some(pid)],
            "{shape}"
        );
        drop((first, main_this, twice, alt));
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// A told client's 10 s to re-attach count from the rebuild, not from when
/// the snapshot was taken as carried timers do: the notice goes out after
/// the dry run, and the wait is the client's from the notice (spec,
/// *Clients*), so a slow dry run takes nothing off it.
#[test]
fn the_reattach_wait_counts_from_the_rebuild() {
    let dir = temp_dir("rwait");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    let (client, _peer) = attach(&mut a, &dir, "s", &[]);
    let taken = std::time::SystemTime::now() - Duration::from_secs(9);
    let since_epoch = taken.duration_since(std::time::UNIX_EPOCH).unwrap();
    let mut s = a.to_snapshot(snap::Meta {
        purpose: snap::Purpose::Upgrade,
        written_us: i64::try_from(since_epoch.as_micros()).unwrap(),
        ..snap::Meta::default()
    });
    for c in &mut s.clients {
        c.token = 0x0123_4567_89ab_cdef;
    }
    let (l, _) = listener(&dir, "b");
    let b = Server::from_snapshot(l, ServerConfig::default(), &s, &mut |r| adopt(&a, r)).unwrap();
    let deadline = b.clients[&client].reattach.unwrap().deadline.unwrap();
    assert!(
        deadline > Instant::now() + Duration::from_secs(5),
        "{:?} left",
        deadline.saturating_duration_since(Instant::now())
    );
    drop((a, b));
    let _ = std::fs::remove_dir_all(dir);
}

/// The OSC 8 id counter goes through a snapshot (tmux's one
/// `hyperlinks_next_external_id`): the snapshot holds a number past every
/// link made so far, status line and popup links included (a pane's own
/// are also kept with its terminal), and the server rebuilt from one
/// numbers on from it, its panes too, so a link drawn after a live upgrade
/// never takes a number still on screen. A snapshot reads the counter and
/// moves it not (two are the same, and persist sees no change), and the
/// counter is the server's: another server's links move it not.
#[test]
fn the_link_counter_crosses_a_snapshot() {
    let dir = temp_dir("linkno");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    let made = a.link_numbers.fresh("http://made-before").number();
    let mut s = a.to_snapshot(snap::Meta::default());
    assert!(s.server.link_number > made, "{}", s.server.link_number);
    let other = vtmux_vt::LinkNumbers::default();
    for _ in 0..3 {
        let _ = other.fresh("http://another-server");
    }
    assert_eq!(
        a.to_snapshot(snap::Meta::default()).server.link_number,
        s.server.link_number
    );
    let far = s.server.link_number + 10_000;
    s.server.link_number = far;
    let b = from_snapshot(&dir, &s).unwrap();
    assert_eq!(b.link_numbers.next(), far);
    let pane = &b.model.panes[&vtmux_core::ids::PaneId(0)].term;
    assert!(pane.link_numbers().same(&b.link_numbers));
    assert!(!b.link_numbers.same(&a.link_numbers));
    assert_eq!(b.link_numbers.fresh("http://after").number(), far);
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

/// A directory in `dir` whose name is not UTF-8 (a byte 0xff), or `None`
/// where the file system refuses one (APFS: `EILSEQ`).
fn non_utf8_dir(dir: &Path) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;
    let bad = dir.join(std::ffi::OsStr::from_bytes(b"x\xff"));
    match std::fs::create_dir(&bad) {
        Ok(()) => Some(bad),
        Err(e) if e.raw_os_error() == Some(rustix::io::Errno::ILSEQ.raw_os_error()) => None,
        Err(e) => panic!("{}: {e}", bad.display()),
    }
}

/// A working directory whose name is not UTF-8 (a byte 0xff) is the same
/// directory after a rebuild: the session's, the pane's (where
/// `respawn-pane` starts it again) and the client's. Ignorable: a reader
/// before takes the lossy text, as every build did.
#[test]
fn a_working_directory_not_utf8_survives_a_rebuild() {
    let dir = temp_dir("cwd");
    let Some(bad) = non_utf8_dir(&dir) else {
        eprintln!("SKIP: this file system refuses a directory name that is not UTF-8");
        let _ = std::fs::remove_dir_all(dir);
        return;
    };
    let mut a = server(&dir);
    ok(&mut a, &bad, &["new-session", "-d", "-s", "s", "cat", "-"]);
    let (client, _peer) = connect(
        &mut a,
        hello(
            &["attach-session", "-t", "s"],
            &bad,
            None,
            ControlMode::Control,
        ),
    );
    a.pending_exits.clear();
    let cwds = |s: &Server| {
        (
            s.model.sessions.values().next().unwrap().cwd.clone(),
            s.model.panes[&PaneId(0)].cwd.clone(),
            s.clients[&client].hello.as_ref().unwrap().cwd.clone(),
        )
    };
    assert_eq!(cwds(&a), (bad.clone(), bad.clone(), bad.clone()));
    let taken = a.to_snapshot(snap::Meta::default());
    assert_eq!(taken.meta.requires, Vec::<String>::new());
    let b = rebuild(&a, &dir, "b");
    assert_eq!(cwds(&b), cwds(&a));
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

/// A pane resized under vim is an ordinary session (a window resize, a
/// split, a zoom, a client attaching at another size): its primary screen,
/// which the alternate one left at its size (Ghostty patch 0028), requires
/// no feature, so the build before reads the snapshot (live upgrade spec,
/// *Required and ignorable fields*; a checkpoint's restore refuses one
/// requiring what it lacks too). It comes back to end the alternate screen
/// as tmux does.
#[test]
fn a_primary_screen_left_at_its_size_requires_nothing() {
    let dir = temp_dir("psize");
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
    let p = vtmux_core::ids::PaneId(0);
    let term = &mut a.model.panes.get_mut(&p).unwrap().term;
    term.feed(b"one\r\ntwo\x1b[?1049h\x1b[Hvim");
    ok(&mut a, &dir, &["resize-window", "-t", "s", "-y", "10"]);
    assert_eq!(a.model.panes[&p].term.rows(), 10);
    for (purpose, form) in [
        (snap::Purpose::Upgrade, crate::snapshot_term::Form::SamePin),
        (snap::Purpose::Upgrade, crate::snapshot_term::Form::Rows),
        (
            snap::Purpose::CheckpointFull,
            crate::snapshot_term::Form::Rows,
        ),
    ] {
        let meta = snap::Meta {
            purpose,
            ..snap::Meta::default()
        };
        let sized = a.to_snapshot_with(meta, Some(form));
        assert_eq!(sized.meta.requires, Vec::<String>::new(), "{purpose:?}");
    }

    let mut b = rebuild(&a, &dir, "b");
    let term = &mut b.model.panes.get_mut(&p).unwrap().term;
    term.feed(b"\x1b[?1049l");
    assert_eq!((term.cols(), term.rows()), (80, 10));
    assert_eq!(term.cursor(), (3, 1));
    drop((a, b));
    let _ = std::fs::remove_dir_all(dir);
}

/// Rows that keep tmux's line flags beyond their cells (Ghostty patch
/// 0034) are an ordinary session: `echo café` and two backspaces in bash
/// (which erases each with `\b\e[K`), a wide character overwritten. Their
/// snapshot requires nothing, so the build before reads it (live upgrade
/// spec, *Required and ignorable fields*), and the rebuilt pane keeps the
/// flags.
#[test]
fn kept_line_flags_require_nothing() {
    for (scene, bytes) in [
        ("bash", "$ echo caf\u{e9}\x08\x1b[K\x08\x1b[K"),
        ("wide", "\u{3042}\rX"),
    ] {
        let dir = temp_dir("lflags");
        let mut a = server(&dir);
        ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
        let p = vtmux_core::ids::PaneId(0);
        let flags = |s: &Server| {
            let t = &s.model.panes[&p].term;
            let o = vtmux_vt::CaptureOpts {
                flags: true,
                ..vtmux_vt::CaptureOpts::default()
            };
            let out = String::from_utf8(vtmux_vt::capture(t, o)).unwrap();
            out.lines().next().unwrap().to_string()
        };
        a.model
            .panes
            .get_mut(&p)
            .unwrap()
            .term
            .feed(bytes.as_bytes());
        assert!(flags(&a).starts_with("X "), "{scene}: {}", flags(&a));
        for (purpose, form) in [
            (snap::Purpose::Upgrade, crate::snapshot_term::Form::SamePin),
            (snap::Purpose::Upgrade, crate::snapshot_term::Form::Rows),
            (
                snap::Purpose::CheckpointFull,
                crate::snapshot_term::Form::Rows,
            ),
        ] {
            let meta = snap::Meta {
                purpose,
                ..snap::Meta::default()
            };
            let taken = a.to_snapshot_with(meta, Some(form));
            assert_eq!(
                taken.meta.requires,
                Vec::<String>::new(),
                "{scene} {purpose:?}"
            );
        }
        let b = rebuild(&a, &dir, "b");
        assert_eq!(flags(&b), flags(&a), "{scene}");
        drop((a, b));
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// A pane's auto-hide scrollbar keeps what it shows and when it hides
/// (`sb_auto_visible`, `sb_auto_hover`, `sb_auto_timer`), under its
/// feature name; one left at the default requires nothing.
#[test]
fn an_auto_hide_scrollbar_stays_as_shown() {
    let dir = temp_dir("sbar");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    let plain = a.to_snapshot(snap::Meta::default());
    assert!(plain.meta.requires.is_empty(), "{:?}", plain.meta.requires);
    let p = vtmux_core::ids::PaneId(0);
    let hide_at = Instant::now() + Duration::from_secs(5);
    a.model.panes.get_mut(&p).unwrap().sb = crate::scrollbar::AutoHide {
        visible: true,
        hover: true,
        hide_at: Some(hide_at),
    };
    let shown = a.to_snapshot(snap::Meta::default());
    assert_eq!(
        shown.meta.requires,
        vec![snap::SCROLLBAR_FEATURE.to_string()]
    );

    let b = rebuild(&a, &dir, "b");
    let sb = b.model.panes[&p].sb;
    assert!(sb.visible && sb.hover, "{sb:?}");
    // The time it had left (counted again from the rebuild), not none or
    // the option's whole timeout.
    let left = sb.hide_at.unwrap();
    assert!(left <= Instant::now() + Duration::from_secs(5), "{left:?}");
    assert!(left + Duration::from_secs(1) > hide_at, "{left:?}");
    drop((a, b));
    let _ = std::fs::remove_dir_all(dir);
}

/// An upgrade's timers count on from when its snapshot was written, not
/// from the rebuild: the dry run and the exec in between are time that
/// passed for them.
#[test]
fn timers_count_from_when_the_snapshot_was_taken() {
    let dir = temp_dir("taken");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    let p = vtmux_core::ids::PaneId(0);
    // Long enough that a host which stalls the test for seconds between
    // here and the checks (a loaded one, the whole suite at once) does not
    // turn the timer's slack into a failure.
    let hide0 = Instant::now() + Duration::from_secs(30);
    a.model.panes.get_mut(&p).unwrap().sb = crate::scrollbar::AutoHide {
        visible: true,
        hover: false,
        hide_at: Some(hide0),
    };
    let ago = |secs| {
        let t = std::time::SystemTime::now() - Duration::from_secs(secs);
        let d = t.duration_since(std::time::UNIX_EPOCH).unwrap();
        i64::try_from(d.as_micros()).unwrap()
    };
    let taken = |written_us, purpose| {
        a.to_snapshot(snap::Meta {
            purpose,
            written_us,
            ..snap::Meta::default()
        })
    };
    let hide_at = |s: &snap::Snapshot| {
        let b = from_snapshot(&dir, s).unwrap();
        b.model.panes[&p].sb.hide_at.unwrap()
    };
    // Written 2 s ago: 28 s left.
    let left = hide_at(&taken(ago(2), snap::Purpose::Upgrade));
    assert!(left <= Instant::now() + Duration::from_secs(28), "{left:?}");
    assert!(left + Duration::from_secs(10) > hide0, "{left:?}");
    // No time, a checkpoint, or a clock far off: counted from the rebuild.
    for s in [
        taken(0, snap::Purpose::Upgrade),
        taken(ago(2), snap::Purpose::CheckpointFull),
    ] {
        let left = hide_at(&s);
        assert!(left > Instant::now() + Duration::from_secs(20), "{left:?}");
    }
    let far = hide_at(&taken(ago(3600), snap::Purpose::Upgrade));
    assert!(far + Duration::from_secs(20) > hide0, "{far:?}");
    drop(a);
    let _ = std::fs::remove_dir_all(dir);
}

/// The finding `low_upgrade_timer_shift_caps` (`review_downgrade_fix`): the
/// shift was capped at 10 s, said to be more than an upgrade takes, but
/// the time between the snapshot and the rebuild grows with the history
/// the new image imports as rows (the dry run may take 3 s and a second
/// per 100k rows, and the new image's import is the same work). A
/// snapshot of a million rows written 12 s ago is shifted by the 12 s; a
/// clock far off still moves nothing far.
#[test]
fn the_shift_cap_grows_with_the_rows_the_snapshot_imports() {
    let ago = |secs| {
        let t = std::time::SystemTime::now() - Duration::from_secs(secs);
        let d = t.duration_since(std::time::UNIX_EPOCH).unwrap();
        i64::try_from(d.as_micros()).unwrap()
    };
    let taken = |written_us, rows| snap::Snapshot {
        meta: snap::Meta {
            purpose: snap::Purpose::Upgrade,
            written_us,
            ..snap::Meta::default()
        },
        panes: vec![snap::Pane {
            term: Some(snap::TermState {
                primary: snap::term::Screen {
                    history: vec![snap::HistoryPage {
                        rows,
                        ..snap::HistoryPage::default()
                    }],
                    ..snap::term::Screen::default()
                },
                ..snap::TermState::default()
            }),
            ..snap::Pane::default()
        }],
        ..snap::Snapshot::default()
    };
    let near =
        |d: Duration, secs: u64| d.abs_diff(Duration::from_secs(secs)) < Duration::from_secs(1);
    let shift = super::since_taken(&taken(ago(12), 1_000_000));
    assert!(near(shift, 12), "{shift:?}");
    let small = super::since_taken(&taken(ago(3600), 0));
    assert!(small < Duration::from_secs(10), "{small:?}");
    let big = super::since_taken(&taken(ago(3600), 1_000_000));
    assert!(big < Duration::from_secs(60), "{big:?}");
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

/// A pane with history `lines` long, compressed where Ghostty can
/// (whether it did), and its capture, taken before: reading history
/// restores the pages it reads.
fn compressed_pane(server: &mut Server, dir: &Path, lines: usize) -> (bool, String) {
    ok(server, dir, &["set", "-g", "history-limit", "10000"]);
    ok(
        server,
        dir,
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
    let pane = server
        .model
        .panes
        .get_mut(&vtmux_core::ids::PaneId(0))
        .unwrap();
    crate::compress::fill(&mut pane.term, lines);
    let want = pane_capture(server, dir);
    let pane = server
        .model
        .panes
        .get_mut(&vtmux_core::ids::PaneId(0))
        .unwrap();
    let now = Instant::now();
    pane.compress.observe(&pane.term, now);
    let later = now + Duration::from_secs(60);
    for _ in 0..10_000 {
        match pane.compress.step(&mut pane.term, later) {
            Some(vtmux_vt::Compression::Pending) => {}
            _ => break,
        }
    }
    (pane.term.compressed_pages() > 0, want)
}

fn pane_capture(server: &mut Server, dir: &Path) -> String {
    run(
        server,
        dir,
        &["capture-pane", "-p", "-e", "-S", "-", "-t", "%0"],
    )
}

/// A server rebuilt from `snapshot` on listener `name`, adopting nothing.
fn restore(dir: &Path, name: &str, snapshot: &snap::Snapshot) -> std::io::Result<Server> {
    let (listening, _) = listener(dir, name);
    Server::from_snapshot(listening, ServerConfig::default(), snapshot, &mut |_| None)
}

fn pane0(server: &Server) -> &crate::model::Pane {
    &server.model.panes[&vtmux_core::ids::PaneId(0)]
}

/// Terminal state goes as the snapshot's purpose says (D27 decisions 17
/// and 23): an upgrade keeps compressed pages in Ghostty's form and says
/// a reader needs this binary's patch stack for them; a full checkpoint
/// writes rows any reader takes; a layout checkpoint writes no screen,
/// and the pane comes back with a fresh terminal.
#[test]
fn terminal_state_goes_as_the_purpose_says() {
    let dir = temp_dir("purpose");
    let mut old = server(&dir);
    let (compressed, want) = compressed_pane(&mut old, &dir, 5000);
    assert!(want.contains("line 4999 some log text"), "{want}");
    let need = crate::snapshot_term::ghostty_pages_requirement();
    let meta = |purpose| snap::Meta {
        purpose,
        ghostty_patches: "stale".into(),
        ..snap::Meta::default()
    };
    let pages = |taken: &snap::Snapshot| -> (usize, usize) {
        let history = &taken.panes[0].term.as_ref().unwrap().primary.history;
        let ghostty = history.iter().filter(|p| p.is_ghostty()).count();
        (ghostty, history.len())
    };

    // Upgrade.
    let up = old.to_snapshot(meta(snap::Purpose::Upgrade));
    assert_eq!(up.meta.ghostty_patches, vtmux_vt::PATCH_STACK);
    let (ghostty, all) = pages(&up);
    assert!(all > 0);
    if compressed {
        assert!(ghostty > 0);
        assert_eq!(up.meta.requires, vec![need.clone()]);
        assert_eq!(
            snap::decode(&snap::encode(&up)),
            Err(snap::Error::Unsupported(need.clone()))
        );
    }
    let features = crate::snapshot_term::reader_features();
    let back = snap::decode_for(&snap::encode(&up), &features, |_| None).unwrap();
    let mut upgraded = restore(&dir, "b", &back).unwrap();
    assert_eq!(
        pane0(&upgraded).term.compressed_pages(),
        pane0(&old).term.compressed_pages()
    );
    assert_eq!(pane_capture(&mut upgraded, &dir), want);
    // Only this binary's stack reads Ghostty's pages; another's is refused
    // with an error, not a panic.
    if compressed {
        let mut foreign = back.clone();
        foreign.meta.ghostty_patches = "another build".into();
        let err = restore(&dir, "c", &foreign).err().unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("another Ghostty build"), "{err}");
    }

    // Full checkpoint: rows only, readable by any reader; the history is
    // resident again and due for the compressor at once.
    let full = old.to_snapshot(meta(snap::Purpose::CheckpointFull));
    assert_eq!(pages(&full).0, 0);
    assert_eq!(full.meta.requires, Vec::<String>::new());
    let before = Instant::now();
    let mut checkpointed =
        restore(&dir, "d", &snap::decode(&snap::encode(&full)).unwrap()).unwrap();
    let pane = pane0(&checkpointed);
    assert_eq!(pane.term.compressed_pages(), 0);
    assert!(
        pane.compress
            .due()
            .is_some_and(|t| t >= before && t <= Instant::now())
    );
    assert_eq!(pane_capture(&mut checkpointed, &dir), want);

    // Layout checkpoint: no terminal; a fresh one, empty, of the pane's
    // size.
    let layout = old.to_snapshot(meta(snap::Purpose::CheckpointLayout));
    assert!(layout.panes.iter().all(|p| p.term.is_none()));
    let mut fresh = restore(&dir, "e", &layout).unwrap();
    assert!(!pane_capture(&mut fresh, &dir).contains("log text"));
    let term = &pane0(&fresh).term;
    assert_eq!((term.cols(), term.rows(), term.history_size()), (80, 24, 0));
    drop((old, upgraded, checkpointed, fresh));
    let _ = std::fs::remove_dir_all(dir);
}

/// A pane terminal that isn't the pane's size, or doesn't decode, is
/// refused as invalid data.
#[test]
fn a_pane_terminal_that_does_not_fit_is_refused() {
    let dir = temp_dir("badterm");
    let mut old = server(&dir);
    ok(
        &mut old,
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
    let good = old.to_snapshot(snap::Meta::default());
    let other = vtmux_vt::PaneTerm::new(40, 10, 100);
    let mut wrong = good.clone();
    wrong.panes[0].term = Some(crate::snapshot_term::term_to_snapshot(
        &other,
        crate::snapshot_term::Form::Rows,
    ));
    let mut short = good.clone();
    short.panes[0].term.as_mut().unwrap().primary.rows.pop();
    for (what, bad) in [("size", wrong), ("rows", short)] {
        let err = restore(&dir, "b", &bad)
            .err()
            .unwrap_or_else(|| panic!("{what}: accepted"));
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{what}");
        assert!(err.to_string().starts_with("%0: "), "{what}: {err}");
    }
    assert!(restore(&dir, "b", &good).is_ok());
    drop(old);
    let _ = std::fs::remove_dir_all(dir);
}

/// Copy mode over a history longer than a snapshot page holds
/// (`snap::row::MAX_PAGE_ROWS`, as `history-limit` lets a pane keep) goes in
/// pages a reader accepts, so the upgrade's dry run does not refuse the
/// snapshot as malformed and the frozen grid comes back whole.
#[test]
fn copy_mode_over_a_long_history_goes_in_pages() {
    let dir = temp_dir("longcopy");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["set", "-g", "history-limit", "100000"]);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    let p = vtmux_core::ids::PaneId(0);
    let lines = snap::row::MAX_PAGE_ROWS as usize + 1000;
    let mut bytes = Vec::new();
    for i in 0..lines {
        bytes.extend_from_slice(format!("line {i}\r\n").as_bytes());
    }
    a.model.panes.get_mut(&p).unwrap().term.feed(&bytes);
    ok(&mut a, &dir, &["copy-mode", "-t", "%0"]);
    let history = |s: &Server| -> Vec<u32> {
        let now = Instant::now();
        match s.model.panes[&p].modes.first() {
            Some(crate::mode::Mode::Copy(c)) => {
                c.export(now).history.iter().map(|pg| pg.rows).collect()
            }
            _ => panic!("not in copy mode"),
        }
    };
    let pages = history(&a);
    assert!(pages.len() > 1, "{pages:?}");
    assert!(
        pages.iter().all(|&n| n <= snap::row::MAX_PAGE_ROWS),
        "{pages:?}"
    );
    let b = rebuild(&a, &dir, "b");
    let rows = |pages: Vec<u32>| pages.iter().map(|&n| u64::from(n)).sum::<u64>();
    assert_eq!(rows(history(&b)), rows(pages));
    drop((a, b));
    let _ = std::fs::remove_dir_all(dir);
}

/// A title goes into the snapshot as `#{pane_title}` reads it, escaped
/// already (`clean_name`); restored without its terminal (a `vtmux-persist
/// layout` checkpoint, or the last resort) it is not escaped again.
#[test]
fn a_title_restored_without_its_terminal_is_not_escaped_again() {
    let dir = temp_dir("title");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    // No TAB: tmux's `select-pane -T` refuses a title with a control
    // character (pane_title_sources), and shows the rest as it was given.
    ok(&mut a, &dir, &["select-pane", "-t", "s", "-T", "a\\b#c"]);
    let title = ["display", "-p", "-t", "s", "#{pane_title}"];
    let before = run(&mut a, &dir, &title);
    assert!(before.contains("\na\\\\b#c\n"), "{before:?}");
    let mut taken = a.to_snapshot(snap::Meta::default());
    for p in &mut taken.panes {
        p.term = None;
    }
    let decoded = snap::decode(&snap::encode(&taken)).unwrap();
    let (l, _) = listener(&dir, "b");
    let mut b =
        Server::from_snapshot(l, ServerConfig::default(), &decoded, &mut |r| adopt(&a, r)).unwrap();
    assert_eq!(run(&mut b, &dir, &title), before);
    drop(b);
    drop(a);
    let _ = std::fs::remove_dir_all(dir);
}

/// The OSC 9;4 progress bar is terminal state a live upgrade carries
/// (`#{pane_pb_state}`, `#{pane_pb_progress}`), never required by name: a
/// program sets one in an ordinary session, so requiring it would refuse
/// every downgrade and strand persistence (spec, *Required and ignorable
/// fields*).
#[test]
fn the_progress_bar_survives_a_rebuild() {
    let dir = temp_dir("progress");
    let mut a = server(&dir);
    ok(&mut a, &dir, &["new-session", "-d", "-s", "s", "cat", "-"]);
    let required = |s: &Server| s.to_snapshot(snap::Meta::default()).meta.requires;
    assert_eq!(required(&a), Vec::<String>::new());
    a.pane_input(vtmux_core::ids::PaneId(0), b"\x1b]9;4;2;64\x1b\\", false);
    let bar = [
        "display",
        "-p",
        "-t",
        "s",
        "#{pane_pb_state}|#{pane_pb_progress}",
    ];
    let before = run(&mut a, &dir, &bar);
    assert!(before.contains("\nerror|64\n"), "{before:?}");
    assert_eq!(required(&a), Vec::<String>::new());
    let mut b = rebuild(&a, &dir, "b");
    assert_eq!(run(&mut b, &dir, &bar), before);
    // Hidden with its progress left over (`9;4;1;50` then `9;4;0`) is
    // carried too, and requires nothing.
    b.pane_input(vtmux_core::ids::PaneId(0), b"\x1b]9;4;0\x1b\\", false);
    let hidden = run(&mut b, &dir, &bar);
    assert!(hidden.contains("\nhidden|64\n"), "{hidden:?}");
    assert_eq!(required(&b), Vec::<String>::new());
    let mut c = rebuild(&b, &dir, "c");
    assert_eq!(run(&mut c, &dir, &bar), hidden);
    drop(c);
    drop(b);
    drop(a);
    let _ = std::fs::remove_dir_all(dir);
}
