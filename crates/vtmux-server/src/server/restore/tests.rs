//! Unit tests of the restore: what the config says before it runs, what a
//! checkpoint becomes before it is imported, and the server rebuilt from
//! it.

use vtmux_core::ids::PaneId;
use vtmux_snapshot as snap;

use super::super::persist::Mode;
use super::super::snapshot::tests::{listener, run, temp_dir};
use super::super::{Server, ServerConfig};
use super::{ConfigSays, Setting, apply_drop, config_says, prepare, rebuild};

/// What `config_says` reads of a config file holding `text`.
fn says(tag: &str, text: &str) -> ConfigSays {
    let dir = temp_dir(tag);
    let conf = dir.join("conf");
    std::fs::write(&conf, text).unwrap();
    let (l, socket) = listener(&dir, "s");
    let config = ServerConfig {
        config_files: vec![conf, dir.join("missing")],
        dry_run: true,
        ..ServerConfig::default()
    };
    let server = Server::new(l, socket, config).unwrap();
    let says = config_says(&server);
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
    says
}

#[test]
fn the_config_is_read_for_the_persistence_options_before_it_runs() {
    let got = says(
        "conf",
        "set -g status on\n\
         set -s vtmux-persist full\n\
         set -g vtmux-persist-drop '@a_* @b'\n\
         set -ga vtmux-persist-drop @c,@d\n\
         set -t x vtmux-persist-drop @session_only\n\
         set vtmux-persist-drop @not_global\n\
         %if \"#{==:1,0}\"\n\
         set -s vtmux-persist off\n\
         %endif\n\
         set -s vtmux-persist screens\n\
         set -so vtmux-persist off\n\
         set -F vtmux-persist '#{?#{==:1,1},layout,full}'\n",
    );
    assert_eq!(
        got,
        ConfigSays {
            persist: Some(Setting::On(Mode::Layout)),
            drop: Some(vec!["@a_*".into(), "@b".into(), "@c".into(), "@d".into()]),
        }
    );
    // `-u` is the default: off, nothing dropped.
    let got = says(
        "unset",
        "set -s vtmux-persist full\nset -su vtmux-persist\nset -gu vtmux-persist-drop\n",
    );
    assert_eq!(
        got,
        ConfigSays {
            persist: Some(Setting::Off),
            drop: Some(Vec::new()),
        }
    );
    // A config that does not mention them, or does not parse, says
    // nothing.
    assert_eq!(says("silent", "set -g status off\n"), ConfigSays::default());
    assert_eq!(
        says("broken", "set -s vtmux-persist full {\n"),
        ConfigSays::default()
    );
}

fn entry(name: &str, value: &str) -> snap::OptionEntry {
    snap::OptionEntry {
        name: name.into(),
        value: snap::OptionValue::Single(snap::Value::String(value.into())),
    }
}

fn globs(globs: &[&str]) -> snap::OptionEntry {
    snap::OptionEntry {
        name: "vtmux-persist-drop".into(),
        value: snap::OptionValue::Array(
            globs
                .iter()
                .enumerate()
                .map(|(i, g)| snap::ArrayItem {
                    key: snap::ArrayKey::Index(u32::try_from(i).unwrap()),
                    value: snap::Value::String((*g).into()),
                })
                .collect(),
        ),
    }
}

fn names(entries: &[snap::OptionEntry]) -> Vec<&str> {
    entries.iter().map(|e| e.name.as_str()).collect()
}

/// Session `a` (window 0, pane 0) has its own `vtmux-persist-drop`;
/// session `b` (window 1, pane 1) goes by the global one.
fn two_sessions() -> snap::Snapshot {
    let users = || vec![entry("@l_x", "1"), entry("@g_x", "2"), entry("@keep", "3")];
    let mut a_opts = users();
    a_opts.push(globs(&["@l_*"]));
    snap::Snapshot {
        server: snap::Server {
            server_options: users(),
            session_options: {
                let mut o = users();
                o.push(globs(&["@g_*"]));
                o
            },
            window_options: users(),
            ..snap::Server::default()
        },
        sessions: [(0, a_opts), (1, users())]
            .into_iter()
            .map(|(id, options)| snap::Session {
                id,
                name: format!("s{id}"),
                windows: vec![snap::Winlink {
                    index: 0,
                    window: id,
                    ..snap::Winlink::default()
                }],
                options,
                ..snap::Session::default()
            })
            .collect(),
        windows: (0..2)
            .map(|id| snap::Window {
                id,
                options: users(),
                ..snap::Window::default()
            })
            .collect(),
        panes: (0..2)
            .map(|id| snap::Pane {
                id,
                window: id,
                options: users(),
                ..snap::Pane::default()
            })
            .collect(),
        ..snap::Snapshot::default()
    }
}

#[test]
fn vtmux_persist_drop_goes_by_each_session_and_the_global_value() {
    let mut s = two_sessions();
    apply_drop(&mut s, &ConfigSays::default());
    let g = ["@l_x", "@keep"];
    assert_eq!(names(&s.server.server_options), g);
    assert_eq!(names(&s.server.window_options), g);
    assert_eq!(
        names(&s.server.session_options),
        ["@l_x", "@keep", "vtmux-persist-drop"]
    );
    // `a` drops `@l_*` (its own), in its window and pane too; `b` the
    // global `@g_*`.
    assert_eq!(
        names(&s.sessions[0].options),
        ["@g_x", "@keep", "vtmux-persist-drop"]
    );
    assert_eq!(names(&s.windows[0].options), ["@g_x", "@keep"]);
    assert_eq!(names(&s.panes[0].options), ["@g_x", "@keep"]);
    assert_eq!(names(&s.sessions[1].options), g);
    assert_eq!(names(&s.windows[1].options), g);
    assert_eq!(names(&s.panes[1].options), g);

    // The config's global value replaces the checkpoint's.
    let mut s = two_sessions();
    let says = ConfigSays {
        drop: Some(vec!["@keep".into()]),
        ..ConfigSays::default()
    };
    apply_drop(&mut s, &says);
    assert_eq!(names(&s.sessions[1].options), ["@l_x", "@g_x"]);
    assert_eq!(
        names(&s.sessions[0].options),
        ["@g_x", "@keep", "vtmux-persist-drop"]
    );
}

/// A real server's `layout` checkpoint, prepared and rebuilt: every pane
/// dead with its status unknown (or kept, for a pane dead already) and
/// the dead line, the start command and path kept, the server this one
/// (pid, socket) with its config still to run.
#[test]
fn a_rebuilt_checkpoint_is_a_starting_server_with_every_pane_dead() {
    let dir = temp_dir("rebuild");
    let (l, socket) = listener(&dir, "s");
    let mut server = Server::new(l, socket.clone(), ServerConfig::default()).unwrap();
    for words in [
        &[
            "new-session",
            "-d",
            "-s",
            "a",
            "-x",
            "80",
            "-y",
            "24",
            "cat",
            "-",
        ][..],
        &["split-window", "-d", "-t", "a", "-c", "/", "cat", "-"],
        &["set", "-s", "vtmux-persist", "layout"],
    ] {
        let out = run(&mut server, &dir, words);
        assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    }
    let mut s = server.checkpoint_snapshot(Mode::Layout);
    s.server.pid = 1;
    s.panes[1].status = Some(snap::ExitStatus {
        code: Some(3),
        ..snap::ExitStatus::default()
    });
    s.panes[1].dead_time = Some(12345);
    let cwd0 = s.panes[0].cwd.clone();
    drop(server);

    let (l, socket) = listener(&dir, "s2");
    let config = ServerConfig {
        environ: vec![("FROM".into(), "client".into())],
        ..ServerConfig::default()
    };
    let s = prepare(s, &ConfigSays::default(), &socket, &config);
    assert_eq!(s.server.pid, i32::try_from(std::process::id()).unwrap());
    assert!(
        s.panes
            .iter()
            .all(|p| p.exited && p.pid == 0 && p.tty.is_empty())
    );
    assert_eq!(s.panes[0].status, Some(snap::ExitStatus::default()));
    // It dies when it is rebuilt, its dead line drawn then.
    assert_eq!(s.panes[0].dead_time, None);
    let since = crate::model::now_epoch();
    let mut server = rebuild(l, config, &s).unwrap();
    let died = server.model.panes[&PaneId(0)].dead_time.unwrap();
    assert!((since..=crate::model::now_epoch()).contains(&died));
    assert!(!server.config_loaded && !server.cfg_finished);
    assert_eq!(server.global_env.get("FROM"), Some("client"));
    let out = run(
        &mut server,
        &dir,
        &[
            "list-panes",
            "-a",
            "-F",
            "#{pane_id} #{pane_dead} [#{pane_dead_status}] #{pane_dead_time} #{pane_start_command} #{pane_start_path} #{socket_path}",
        ],
    );
    let want = format!(
        "\n%0 1 [] {died} cat - {cwd0} {sock}\n%1 1 [3] 12345 cat - / {sock}\n--stderr--\n--exit Some(0)--\n",
        sock = socket.display()
    );
    assert!(out.ends_with(&want), "{out}");
    // `layout` keeps no screen: a pane dead already gets its line again.
    for p in ["%0", "%1"] {
        let out = run(&mut server, &dir, &["capture-pane", "-p", "-t", p]);
        assert!(out.contains("Pane is dead"), "{p}: {out}");
    }
    // `layout` keeps no title: a new pane's, the host name.
    let out = run(
        &mut server,
        &dir,
        &["display", "-p", "-t", "%1", "[#{pane_title}]"],
    );
    assert!(
        out.contains(&format!("\n[{}]\n", server.info.host)),
        "{out}"
    );
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

/// What `p` shows of a rebuilt server: its rows (`capture-pane -p`) and
/// its history size.
fn shown(server: &mut Server, dir: &std::path::Path, p: &str) -> (Vec<String>, String) {
    let out = run(server, dir, &["capture-pane", "-p", "-t", p]);
    let rows = out
        .lines()
        .skip(1)
        .take_while(|l| *l != "--stderr--")
        .map(str::to_owned)
        .collect();
    let history = run(server, dir, &["display", "-p", "-t", p, "#{history_size}"]);
    (rows, history.lines().nth(1).unwrap_or_default().to_owned())
}

/// A `full` checkpoint rebuilt: a pane whose last row is blank shows its
/// saved rows where they were, the dead line on that row and nothing
/// scrolled into history; one whose last row holds output scrolls it up a
/// row as tmux's dead line does (server-fn.c `server_destroy_pane`); one
/// dead already shows its saved screen, its dead line on it, unchanged.
#[test]
fn a_rebuilt_full_checkpoint_keeps_each_saved_screen_in_place() {
    let dir = temp_dir("rebuild-full");
    let (l, socket) = listener(&dir, "s");
    let mut server = Server::new(l, socket.clone(), ServerConfig::default()).unwrap();
    for words in [
        &[
            "new-session",
            "-d",
            "-s",
            "a",
            "-x",
            "20",
            "-y",
            "4",
            "cat",
            "-",
        ][..],
        &["new-window", "-d", "-t", "a", "cat", "-"],
        &["new-window", "-d", "-t", "a", "cat", "-"],
        &["set", "-s", "vtmux-persist", "full"],
    ] {
        let out = run(&mut server, &dir, words);
        assert!(out.ends_with("--exit Some(0)--\n"), "{out}");
    }
    for (p, bytes) in [
        (0, &b"one\r\ntwo\r\n"[..]),
        (1, b"one\r\ntwo\r\nthree\r\nfour"),
        (2, b"saved\r\n\r\n\r\nPane is dead (old)\x1b[4;1H"),
    ] {
        server
            .model
            .panes
            .get_mut(&PaneId(p))
            .unwrap()
            .term
            .feed(bytes);
    }
    let mut s = server.checkpoint_snapshot(Mode::Full);
    s.panes[2].dead_time = Some(12345);
    drop(server);

    let (l, socket) = listener(&dir, "s2");
    let s = prepare(s, &ConfigSays::default(), &socket, &ServerConfig::default());
    let mut server = rebuild(l, ServerConfig::default(), &s).unwrap();
    let (rows, history) = shown(&mut server, &dir, "%0");
    assert_eq!(rows[..3], ["one", "two", ""], "{rows:?}");
    assert!(rows[3].starts_with("Pane is dead (, "), "{rows:?}");
    assert_eq!(history, "0");
    let (rows, history) = shown(&mut server, &dir, "%1");
    assert_eq!(rows[..3], ["two", "three", "four"], "{rows:?}");
    assert!(rows[3].starts_with("Pane is dead (, "), "{rows:?}");
    assert_eq!(history, "1");
    assert_eq!(
        shown(&mut server, &dir, "%2"),
        (
            ["saved", "", "", "Pane is dead (old)"]
                .map(str::to_owned)
                .to_vec(),
            "0".to_owned()
        )
    );
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
