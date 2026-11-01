//! `display-panes` (window-panes.c, `panes-mode`): a pane mode that shows
//! every pane of a window as a number over a copy of its screen and runs a
//! command on the pane whose number is typed.
//!
//! `cmd_choose_tree_exec` enters it with `window_pane_set_mode`
//! (cmd-choose-tree.c:100-113,148-152): `WINDOW_MODE_NO_STACK` (a mode
//! under it is not kept: entering another replaces it) and
//! `HIDE_PANE_STATUS`. Unless `-Z`, `init` zooms the pane (window-panes.c
//! :941-948) and the mode's exit unzooms it again if the window was not
//! zoomed before; a preview of the pane's screen is kept from before the
//! zoom.
//!
//! The screen is drawn by [`crate::render::panes`] each frame from the
//! pane screens kept in the mode, which tmux draws once at `init` and at
//! every resize ([`PanesMode::frozen`]); option and format changes show at
//! the next frame, where tmux keeps its drawing. The mouse (`KEYC_MOUSEDOWN1_PANE`) and
//! the floating layer are not here yet, so neither the click areas nor the
//! floating borders are.
//!
//! `HIDE_PANE_STATUS` gives the zoomed pane the row its status line would
//! have taken under `pane-border-status` (window.c:2936-2940 into
//! `layout_fix_panes`: [`crate::model::Model::fix_panes`] and the scene).
//! `FILL_WINDOW` and `HIDE_SCROLLBARS` are for scrollbars, which vtmux
//! does not have.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use vtmux_core::cmd::{ArgValue, Args, CmdError, CommandList};
use vtmux_core::error::EdgeError;
use vtmux_core::ids::{PaneId, SessionId, WindowId};
use vtmux_core::keys::Key;
use vtmux_core::options::TreeId;
use vtmux_vt::Snapshot;

use super::{Mode, PaneMode, copy};
use crate::cmdq::Source;
use crate::server::{ClientId, Server};
use crate::target::{Found, Origin};

/// `args_make_commands_state`'s command: a `{ }` block, or text the pane's
/// id is filled into (`select-pane -t "%%%"` when none is given).
#[derive(Clone, Debug)]
enum Template {
    Block(CommandList),
    Text(String),
}

/// `struct window_panes_modedata`.
#[derive(Clone, Debug)]
pub struct PanesMode {
    /// The session of the command's target (`data->session`).
    pub session: SessionId,
    /// `-s`, or the target's, session and window the numbers are for.
    pub source_session: SessionId,
    pub source_window: WindowId,
    /// `-N`: keys other than `q` and Escape do nothing.
    pub ignore_keys: bool,
    /// The window was not zoomed and `-Z` was not given: leaving unzooms
    /// (`data->zoomed == 0`).
    unzoom_on_exit: bool,
    /// The target pane's screen from before the zoom (`data->preview`).
    pub preview: Option<Snapshot>,
    /// Every pane's screen as it was when the numbers were last drawn
    /// (window-panes.c draws at `init` and in `resize`, not again).
    pub frozen: BTreeMap<PaneId, Snapshot>,
    /// The size of that drawing: the pane's.
    pub size: (u16, u16),
    /// When the timeout ends the mode; none for `-d 0`.
    deadline: Option<Instant>,
    template: Template,
    source: Option<Rc<Source>>,
    /// `-k`: the pane is killed when the mode ends.
    pub kill: bool,
}

impl PaneMode for PanesMode {
    fn name(&self) -> &'static str {
        "panes-mode"
    }

    /// No key table: keys reach [`Server::panes_key`].
    fn key_table(&self, _vi: bool) -> Option<&'static str> {
        None
    }

    fn takes_commands(&self) -> bool {
        false
    }

    fn set_prefix(&mut self, _n: u32) {}

    fn command(
        &mut self,
        _env: &copy::Env,
        name: &str,
        _args: &[String],
        _read_only_client: bool,
    ) -> Result<copy::Outcome, copy::CommandError> {
        Err(copy::CommandError::NotImplemented(name.to_owned()))
    }
}

/// `cmd_choose_tree_exec` for `display-panes`: `window_pane_set_mode` and
/// `window_panes_init` (window-panes.c:906-975).
///
/// # Errors
/// An invalid `-d`.
pub fn enter(
    server: &mut Server,
    p: PaneId,
    args: &Args,
    found: Found,
    source: Found,
    cmd_source: Option<Rc<Source>>,
) -> Result<(), CmdError> {
    let Some(pane) = server.model.panes.get(&p) else {
        return Ok(());
    };
    // window_pane_set_mode: already in it, nothing happens.
    if matches!(pane.modes.first(), Some(Mode::Panes(_))) {
        return Ok(());
    }
    let w = pane.window;
    let delay = if args.flag('d') {
        args.number('d', 0, i64::from(u32::MAX))
            .map_err(|cause| EdgeError::BadNumber {
                what: "delay",
                cause,
            })?
    } else {
        server
            .model
            .number_option(TreeId::Window(w), "display-panes-time")
    };
    let session = found.session.ok_or(EdgeError::NoCurrentTarget)?;
    let (source_session, source_window) = if args.flag('s') {
        (source.session, source.window)
    } else {
        (found.session, found.window)
    };
    let template = match args.positionals.first() {
        Some(ArgValue::Commands(list)) => Template::Block(list.clone()),
        Some(value) => Template::Text(value.text().into_owned()),
        None => Template::Text("select-pane -t \"%%%\"".to_owned()),
    };
    let zoom = !args.flag('Z')
        && !server
            .model
            .windows
            .get(&w)
            .is_some_and(crate::model::Window::zoomed);
    let mode = PanesMode {
        session,
        source_session: source_session.ok_or(EdgeError::NoCurrentTarget)?,
        source_window: source_window.ok_or(EdgeError::NoCurrentTarget)?,
        ignore_keys: args.flag('N'),
        unzoom_on_exit: zoom,
        preview: zoom.then(|| vtmux_vt::snapshot(&server.model.panes[&p].term)),
        frozen: BTreeMap::new(),
        size: (0, 0),
        deadline: (delay != 0)
            .then(|| Instant::now() + Duration::from_millis(u64::try_from(delay).unwrap_or(0))),
        template,
        source: cmd_source,
        kill: args.flag('k'),
    };
    // A mode that is not stacked goes first (window.c:1755-1756).
    if server.model.panes[&p]
        .modes
        .first()
        .is_some_and(Mode::no_stack)
    {
        server.reset_mode(p);
    }
    let pane = server.model.panes.get_mut(&p).expect("pane");
    let previous = pane.modes.first().map(Mode::name);
    pane.modes.insert(0, Mode::Panes(Box::new(mode)));
    // window_panes_init: `window_zoom(wp)` (a single-pane window stays as
    // it is) and `server_redraw_window`.
    if zoom && server.model.zoom(w, p) {
        server.model.redraw(crate::model::Redraw::Window(w));
    }
    server.model.panes_redrawn(p);
    server.model.mode_changed(p);
    server
        .model
        .fire_mode_changed(p, previous, Some("panes-mode"), true);
    Ok(())
}

impl crate::model::Model {
    /// `window_panes_draw_screen`'s reading of the panes: every pane of
    /// the source window as it is now, kept in pane `p`'s mode.
    fn panes_redrawn(&mut self, p: PaneId) {
        let Some(Mode::Panes(m)) = self.panes.get(&p).and_then(|x| x.modes.first()) else {
            return;
        };
        let Some(window) = self.windows.get(&m.source_window) else {
            return;
        };
        let frozen = window
            .panes
            .panes
            .iter()
            .filter_map(|q| Some((*q, vtmux_vt::snapshot(&self.panes.get(q)?.term))))
            .collect();
        let term = &self.panes[&p].term;
        let size = (term.cols(), term.rows());
        if let Some(Mode::Panes(m)) = self.panes.get_mut(&p).and_then(|x| x.modes.first_mut()) {
            m.frozen = frozen;
            m.size = size;
        }
    }

    /// `window_panes_resize` through `window_pane_resize`: a pane of window
    /// `w` in panes mode that changed size is drawn again.
    pub(crate) fn panes_resized(&mut self, w: WindowId) {
        let Some(window) = self.windows.get(&w) else {
            return;
        };
        let stale: Vec<PaneId> = window
            .panes
            .panes
            .iter()
            .copied()
            .filter(|q| {
                self.panes.get(q).is_some_and(|x| {
                    matches!(x.modes.first(), Some(Mode::Panes(m))
                        if m.size != (x.term.cols(), x.term.rows()))
                })
            })
            .collect();
        for q in stale {
            self.panes_redrawn(q);
        }
    }
}

impl PanesMode {
    /// `window_panes_free`: unzoom if the mode zoomed the window.
    pub(crate) fn leave(&self, model: &mut crate::model::Model, p: PaneId) {
        let Some(w) = model.panes.get(&p).map(|x| x.window) else {
            return;
        };
        // server_unzoom_window, then server_redraw_window.
        model.redraw(crate::model::Redraw::Window(w));
        if self.unzoom_on_exit {
            model.unzoom(w);
        }
    }

    /// The soonest the timeout ends the mode.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
}

/// `window_panes_get_source`: the session that shows the source window and
/// the window's index in it, falling back to the command's session.
#[must_use]
pub fn source_link(model: &crate::model::Model, m: &PanesMode) -> Option<(SessionId, u32)> {
    model.windows.get(&m.source_window)?;
    let index_in = |s: SessionId| {
        model
            .sessions
            .get(&s)?
            .windows
            .iter()
            .find_map(|(i, w)| (*w == m.source_window).then_some(*i))
    };
    if let Some(i) = index_in(m.source_session) {
        return Some((m.source_session, i));
    }
    model.sessions.get(&m.session)?;
    Some((
        m.session,
        index_in(m.session).unwrap_or(model.sessions[&m.session].current),
    ))
}

impl Server {
    /// The panes mode on pane `p`, if it is the shown mode.
    fn panes_of(&self, p: PaneId) -> Option<&PanesMode> {
        let Mode::Panes(m) = self.model.panes.get(&p)?.modes.first()? else {
            return None;
        };
        Some(m)
    }

    /// `window_panes_key` (window-panes.c:1080-1099), for a key from client
    /// `c`: `q` and Escape leave; the number or letter of a pane runs the
    /// command on it and leaves; any other key leaves unless `-N`.
    pub(crate) fn panes_key(&mut self, p: PaneId, c: ClientId, key: Key) {
        let key = key.without_flags();
        if key == Key::ascii(0x1b) || key == Key::ascii(b'q') {
            self.reset_mode(p);
            return;
        }
        let Some(m) = self.panes_of(p) else { return };
        let ignore = m.ignore_keys;
        let target = if ignore || key.is_mouse() {
            None
        } else {
            self.panes_key_pane(m, key)
        };
        let Some(target) = target else {
            if !ignore && !key.is_mouse() {
                self.reset_mode(p);
            }
            return;
        };
        let w = self.model.panes.get(&p).map(|x| x.window);
        if let Some(w) = w
            && self
                .model
                .windows
                .get(&w)
                .is_some_and(crate::model::Window::zoomed)
        {
            self.model.unzoom(w);
        }
        if let Some(m) = self.panes_of(p).cloned() {
            self.panes_run(&m, c, target);
        }
        self.reset_mode(p);
    }

    /// `window_panes_key_pane`: digits are indexes 0 to 9, `a` to `z`
    /// 10 to 35; a key with modifiers is none.
    fn panes_key_pane(&self, m: &PanesMode, key: Key) -> Option<PaneId> {
        let k = key.code();
        let index = if (u64::from(b'0')..=u64::from(b'9')).contains(&k) {
            k - u64::from(b'0')
        } else if key.modifiers() == 0
            && (u64::from(b'a')..=u64::from(b'z')).contains(&key.base().code())
        {
            10 + key.base().code() - u64::from(b'a')
        } else {
            return None;
        };
        let w = self.model.windows.get(&m.source_window)?;
        let base = u64::from(self.model.pane_base_index(m.source_window));
        let at = usize::try_from(index.checked_sub(base)?).ok()?;
        w.panes.panes.get(at).copied()
    }

    /// `window_panes_run_command`: the template with the pane's id
    /// (`%N`) filled in, queued for the client; a template that does not
    /// parse is an error message there.
    fn panes_run(&mut self, m: &PanesMode, c: ClientId, target: PaneId) {
        let id = target.to_string();
        let calls = match &m.template {
            Template::Block(list) => Ok(vtmux_core::cmd::template::replace_list(list, &[id]).calls),
            Template::Text(text) => {
                let filled = vtmux_core::cmd::template::replace(text, &id, 1);
                let origin = Origin {
                    attached: self.client_session(c),
                    ..Origin::default()
                };
                crate::commands::run_shell::make_commands(
                    self,
                    Some(c),
                    origin,
                    m.source.as_deref(),
                    &ArgValue::Str(filled),
                )
            }
        };
        match calls {
            Ok(calls) => self.client_append(Some(c), calls),
            Err(msg) => self.client_error_message(Some(c), &msg),
        }
    }

    /// The soonest timeout of a panes mode.
    pub(crate) fn panes_deadline(&self) -> Option<Instant> {
        self.model
            .panes
            .values()
            .filter_map(|p| match p.modes.first() {
                Some(Mode::Panes(m)) => m.deadline(),
                _ => None,
            })
            .min()
    }

    /// `window_panes_timer_callback`: the display time is up, the mode goes.
    pub(crate) fn panes_timers(&mut self, now: Instant) {
        let due: Vec<PaneId> = self
            .model
            .panes
            .iter()
            .filter(|(_, p)| {
                matches!(p.modes.first(), Some(Mode::Panes(m)) if m.deadline().is_some_and(|d| d <= now))
            })
            .map(|(&id, _)| id)
            .collect();
        for p in due {
            self.reset_mode(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::prompt::tests::setup;

    fn enter_for(server: &mut Server, p: PaneId, words: &[&str]) -> Result<(), String> {
        let mut argv: Vec<Vec<u8>> = vec![b"display-panes".to_vec()];
        argv.extend(words.iter().map(|w| w.as_bytes().to_vec()));
        let calls = vtmux_core::cmd::parse_argv_bytes(&argv).map_err(|e| e.to_string())?;
        let found = Found::from_pane(&server.model, p);
        enter(server, p, &calls[0].args, found, found, None).map_err(|e| e.to_string())
    }

    /// window-panes.c:906-975: a single pane window has nothing to zoom, so
    /// the mode keeps the window as it is and leaving does not unzoom; `-d`
    /// is a number from 0; 0 has no timeout.
    #[test]
    fn entry_and_timeout() {
        let (mut server, p, _, _, dir) = setup();
        server.reset_mode_all(p);
        assert_eq!(
            enter_for(&mut server, p, &["-d", "x"]),
            Err("delay invalid".into())
        );
        assert_eq!(
            enter_for(&mut server, p, &["-d", "-1"]),
            Err("delay too small".into())
        );
        assert!(server.model.panes[&p].modes.is_empty());
        enter_for(&mut server, p, &["-d", "0"]).unwrap();
        assert_eq!(
            crate::mode::PaneMode::name(server.model.panes[&p].modes.first().unwrap()),
            "panes-mode"
        );
        assert!(server.panes_deadline().is_none());
        server.reset_mode_all(p);
        enter_for(&mut server, p, &["-d", "50"]).unwrap();
        let at = server.panes_deadline().expect("a timeout");
        server.panes_timers(at.checked_sub(Duration::from_millis(1)).unwrap());
        assert_eq!(server.model.panes[&p].modes.len(), 1);
        server.panes_timers(at);
        assert!(server.model.panes[&p].modes.is_empty());
        assert!(
            !server
                .model
                .windows
                .values()
                .any(crate::model::Window::zoomed)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// window-panes.c:1022-1037: digits are pane indexes, letters from 10,
    /// and a key with a modifier is none.
    #[test]
    fn keys_name_panes() {
        let (mut server, p, _, _, dir) = setup();
        server.reset_mode_all(p);
        enter_for(&mut server, p, &["-d", "0"]).unwrap();
        let m = server.panes_of(p).unwrap().clone();
        assert_eq!(server.panes_key_pane(&m, Key::ascii(b'0')), Some(p));
        assert_eq!(server.panes_key_pane(&m, Key::ascii(b'1')), None);
        assert_eq!(server.panes_key_pane(&m, Key::ascii(b'a')), None);
        assert_eq!(server.panes_key_pane(&m, Key::ascii(b'0').ctrl()), None);
        assert_eq!(server.panes_key_pane(&m, Key::ascii(b'a').meta()), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
