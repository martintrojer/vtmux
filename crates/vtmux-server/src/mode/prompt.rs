//! A mode's own prompt (`mode_tree_set_prompt`, mode-tree.c:1130-1178):
//! a [`Prompt`](crate::prompt::Prompt) drawn on the mode screen's last
//! row, or row 0 with `status-position top` (`mode_tree_draw_prompt`,
//! mode-tree.c:1046-1070), not on the status line and not over the pane
//! like `command-prompt -P`. Its keys come from the mode (`mode_tree_key`,
//! mode-tree.c:1519-1566), before the mode's own handling; a key the
//! prompt does not take goes on to the mode.
//!
//! The prompt belongs to the mode that opened it: it is kept on the pane
//! with the number of modes then on the stack, shown and fed only while
//! that many are, and dropped when the stack falls below that.

use std::fmt::Debug;

use vtmux_core::ids::PaneId;

use crate::model::Pane;
use crate::open_prompt::{Line, Reply};
use crate::prompt::OpenPrompt;
use crate::server::{ClientId, Server};

/// What the mode that opened a prompt does with it (`mode_tree_prompt`'s
/// `inputcb` and `freecb`).
pub(crate) trait ModePromptOwner: Debug {
    /// `inputcb`: a finished line, or an incremental prompt's edit. `c` is
    /// the client the key came from (`mtp->c`).
    fn input(
        &mut self,
        server: &mut Server,
        pane: PaneId,
        c: Option<ClientId>,
        line: Line,
    ) -> Reply;

    /// `freecb`: the prompt is gone.
    fn free(self: Box<Self>, _server: &mut Server) {}
}

/// A mode's prompt on its pane (`mtd->prompt` with `mtd->prompt_top`).
#[derive(Debug)]
pub struct ModePrompt {
    pub open: OpenPrompt,
    /// `mtd->prompt_top`: `status-position top` when it was set.
    pub top: bool,
    /// The modes on the pane's stack when it was set.
    depth: usize,
}

impl ModePrompt {
    pub(crate) fn new(open: OpenPrompt, top: bool, depth: usize) -> Self {
        Self { open, top, depth }
    }

    /// Whether the mode that set it is the pane's shown mode.
    #[must_use]
    pub fn shown(&self, pane: &Pane) -> bool {
        pane.modes.len() == self.depth
    }

    /// The mode that set it has left the stack.
    pub(crate) fn gone(&self, pane: &Pane) -> bool {
        pane.modes.len() < self.depth
    }
}

impl Server {
    /// `mode_tree_clear_prompt`: the prompt goes and its owner is freed.
    pub(crate) fn mode_prompt_clear(&mut self, p: PaneId) {
        let Some(pane) = self.model.panes.get_mut(&p) else {
            return;
        };
        let had = pane.mode_prompt.take().is_some();
        let owner = pane.mode_prompt_owner.take();
        if had {
            self.model.redraw_panes.insert(p);
        }
        if let Some(owner) = owner {
            owner.free(self);
        }
    }

    /// A mode left pane `p`'s stack: its prompt goes with it.
    pub(crate) fn mode_prompt_gone(&mut self, p: PaneId) {
        if self
            .model
            .panes
            .get(&p)
            .and_then(|x| x.mode_prompt.as_ref().map(|m| m.gone(x)))
            == Some(true)
        {
            self.mode_prompt_clear(p);
        }
    }

    /// A key for pane `p`'s shown mode from client `c`
    /// (`window_pane_key`, window.c:2080-2092, reaching `mode_tree_key`):
    /// the mode's prompt takes it first. `true` when the prompt used it up;
    /// `false` when the mode should handle it.
    pub(crate) fn mode_prompt_key(
        &mut self,
        p: PaneId,
        c: Option<ClientId>,
        key: vtmux_core::keys::Key,
    ) -> bool {
        let Some(c) = c else { return false };
        let shown = self
            .model
            .panes
            .get(&p)
            .is_some_and(|x| x.mode_prompt.as_ref().is_some_and(|m| m.shown(x)));
        shown && self.prompt_key_at(crate::open_prompt::Place::Mode(p), c, key)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use vtmux_core::ids::SessionId;
    use vtmux_core::keys::Key;

    use super::*;
    use crate::mode::CopyMode;
    use crate::open_prompt::Owner;
    use crate::prompt::Kind;
    use crate::prompt_history::PromptType;
    use crate::render::Caps;
    use crate::server::{NewSession, ServerConfig};
    use crate::target::Found;

    /// Lines the test owner was told.
    type Seen = Rc<RefCell<Vec<String>>>;

    #[derive(Debug)]
    struct Recorder {
        seen: Seen,
        close: bool,
    }

    impl ModePromptOwner for Recorder {
        fn input(
            &mut self,
            _server: &mut Server,
            _pane: PaneId,
            _c: Option<ClientId>,
            line: Line,
        ) -> Reply {
            // Like mode-tree's callbacks, a cancelled prompt is closed.
            let cancelled = matches!(line, Line::Done(None));
            self.seen.borrow_mut().push(match line {
                Line::Done(Some(s)) => format!("done {s}"),
                Line::Done(None) => "cancel".to_owned(),
                Line::Changed(s) => format!("changed {s}"),
                Line::Move => "move".to_owned(),
            });
            if self.close || cancelled {
                Reply::Close
            } else {
                Reply::Continue
            }
        }

        fn free(self: Box<Self>, _server: &mut Server) {
            self.seen.borrow_mut().push("free".to_owned());
        }
    }

    /// A server with session `a` (one `cat` pane, 20x5) whose pane is in
    /// copy mode, and a client to key from.
    pub(crate) fn setup() -> (Server, PaneId, SessionId, ClientId, std::path::PathBuf) {
        // One directory per test thread: each test removes its own when done.
        let thread = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
        let dir = std::env::temp_dir().join(format!("vtmux-mp-{}-{thread}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("s");
        let _ = std::fs::remove_file(&socket);
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let mut server = Server::new(listener, socket.clone(), ServerConfig::default()).unwrap();
        let s = server
            .create_session(NewSession {
                name: Some("a".into()),
                window_name: Some("w".into()),
                argv: vec!["cat".into()],
                cwd: dir.clone(),
                cols: 20,
                rows: 5,
                client_env: Vec::new(),
                extra_env: Vec::new(),
                update_env: false,
                default_size: None,
            })
            .unwrap();
        let p = server.model.current_of(s).unwrap().2;
        server.model.set_copy_mode(p, false, |m| {
            CopyMode::new(
                &m.panes[&p].term,
                (20, 5),
                false,
                (false, false),
                false,
                None,
            )
        });
        (server, p, s, ClientId(0), dir)
    }

    fn open(server: &mut Server, p: PaneId, kind: Kind, close: bool) -> Seen {
        let seen = Seen::default();
        let mut editor = crate::prompt::Prompt::new("Kill? ".into(), "", kind, PromptType::Command);
        editor.no_format = true;
        let owner = Owner::Mode(Box::new(Recorder {
            seen: Rc::clone(&seen),
            close,
        }));
        server.mode_prompt_set(p, None, editor, Found::default(), owner);
        seen
    }

    /// The pane's drawn screen rows, as text.
    fn rows(server: &Server, p: PaneId, s: SessionId) -> (Vec<String>, Option<(u16, u16)>) {
        let source = crate::render::window::Source {
            model: &server.model,
            info: &server.info,
            env: &server.global_env,
        };
        let caps = Caps {
            rgb: false,
            palette_colours: 256,
            theme_palette: false,
            sync: false,
            bce: true,
            xterm: true,
            colours: 256,
            overlay: false,
        };
        let (_, draw) = source.window(s, 20, 5, caps, None).unwrap();
        let content = &draw.panes[&p].content;
        let text = (0..content.rows)
            .map(|y| {
                (0..content.cols)
                    .map(|x| {
                        let c = &content.cell(x, y).unwrap().text;
                        if c.is_empty() {
                            " ".to_owned()
                        } else {
                            c.clone()
                        }
                    })
                    .collect::<String>()
            })
            .collect();
        (text, content.cursor)
    }

    /// mode-tree.c:1046-1070: the prompt is drawn on the mode screen's last
    /// row, its cursor after the input; rows above are the mode's own.
    #[test]
    fn prompt_is_on_the_last_row_of_the_mode_screen() {
        let (mut server, p, s, _, dir) = setup();
        let seen = open(&mut server, p, Kind::Single, true);
        let (rows, cursor) = rows(&server, p, s);
        assert!(rows[4].starts_with("Kill? "), "{rows:?}");
        assert!(!rows[3].contains("Kill"), "{rows:?}");
        assert_eq!(cursor, Some((6, 4)));
        assert!(seen.borrow().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `status-position top` puts it on row 0 (`prompt_top`).
    #[test]
    fn prompt_follows_status_position_top() {
        let (mut server, p, s, _, dir) = setup();
        // status-position: top is choice 0.
        server.model.options.set_table_value(
            vtmux_core::options::TreeId::GlobalSession,
            "status-position",
            vtmux_core::options::Value::Choice(0),
        );
        open(&mut server, p, Kind::Single, true);
        let (rows, cursor) = rows(&server, p, s);
        assert!(rows[0].starts_with("Kill? "), "{rows:?}");
        assert!(!rows[4].contains("Kill"), "{rows:?}");
        assert_eq!(cursor, Some((6, 0)));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The prompt engine feeds it: a `-1` style prompt answers on one key,
    /// the owner is told and then freed once, and the row is drawn again
    /// without the prompt.
    #[test]
    fn keys_reach_the_owner_and_close_the_prompt() {
        let (mut server, p, s, c, dir) = setup();
        let seen = open(&mut server, p, Kind::Single, true);
        assert!(server.mode_prompt_key(p, Some(c), Key::from_char('y')));
        assert_eq!(*seen.borrow(), ["done y", "free"]);
        assert!(server.model.panes[&p].mode_prompt.is_none());
        let (rows, _) = rows(&server, p, s);
        assert!(!rows[4].contains("Kill"), "{rows:?}");
        // No prompt: the key is the mode's.
        assert!(!server.mode_prompt_key(p, Some(c), Key::from_char('y')));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An editing prompt keeps what is typed until Enter (`PROMPT_NOFORMAT`
    /// input kept as typed, `#` included) and Escape cancels.
    #[test]
    fn line_prompt_edits_and_cancels() {
        let (mut server, p, s, c, dir) = setup();
        let seen = open(&mut server, p, Kind::Line, false);
        for ch in "a#b".chars() {
            assert!(server.mode_prompt_key(p, Some(c), Key::from_char(ch)));
        }
        let (rows, cursor) = rows(&server, p, s);
        assert!(rows[4].starts_with("Kill? a#b"), "{rows:?}");
        assert_eq!(cursor, Some((9, 4)));
        assert!(server.mode_prompt_key(p, Some(c), Key::from_name("Enter").unwrap()));
        assert_eq!(*seen.borrow(), ["done a#b"]);
        // The owner kept the prompt open (`PROMPT_CONTINUE`); Escape ends it.
        assert!(server.mode_prompt_key(p, Some(c), Key::from_name("Escape").unwrap()));
        assert_eq!(seen.borrow()[1..], ["cancel", "free"]);
        assert!(server.model.panes[&p].mode_prompt.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The prompt belongs to the mode that opened it: with another mode
    /// pushed on top it neither draws nor takes keys, and when its mode
    /// leaves the stack it goes, its owner freed.
    #[test]
    fn prompt_goes_with_its_mode() {
        let (mut server, p, _, c, dir) = setup();
        let seen = open(&mut server, p, Kind::Line, false);
        server.reset_mode(p);
        assert!(server.model.panes[&p].mode_prompt.is_none());
        assert_eq!(*seen.borrow(), ["free"]);
        assert!(!server.mode_prompt_key(p, Some(c), Key::from_char('x')));
        let _ = std::fs::remove_dir_all(dir);
    }
}
