//! Popups (popup.c): a terminal running a job on its own PTY, drawn over
//! one client's screen ([`crate::render::popup`]), taking that client's
//! keys while it is up.
//!
//! The command that opened it waits (`CMD_RETURN_WAIT`) until the popup
//! closes; a command client then exits with the job's status. With `-E`
//! the popup closes when the job exits, with `-EE` only when it exits 0;
//! otherwise it stays until Escape or C-c, or with `-k` any key once the
//! job is gone.

use std::path::PathBuf;

use mio::Token;
use vtmux_core::ids::SessionId;
use vtmux_core::keys::Key;
use vtmux_core::options::TreeId;
use vtmux_core::style::{CellStyle, Style};
use vtmux_vt::PaneTerm;

use crate::cmdq::StepId;
use crate::jobs::JobStatus;
use crate::pane_io::{PaneIo, ReadResult};
use crate::render::popup::BoxLines;
use crate::server::{ClientId, Server};
use crate::sys;

/// Popup PTY tokens: `POPUP_BASE + client id`, between the jobs' and the
/// clients'.
pub(crate) const POPUP_BASE: usize = (1 << 31) + (1 << 30);

/// When the popup closes by itself (`POPUP_CLOSEEXIT`,
/// `POPUP_CLOSEEXITZERO`, `POPUP_CLOSEANYKEY`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CloseRules {
    /// `-E`: when the job exits.
    pub on_exit: bool,
    /// `-EE`: when the job exits 0.
    pub on_exit_zero: bool,
    /// `-k`: on any key once the job has exited.
    pub any_key: bool,
}

/// What a popup runs.
#[derive(Debug)]
pub enum Command {
    /// `<shell> -c <command>` (`default-command`, or the one argument).
    Shell(String),
    /// The shell alone, or more than one argument, `execvp`ed.
    Argv(Vec<String>),
}

/// `popup_display`'s arguments.
#[derive(Debug)]
pub struct Open {
    pub session: SessionId,
    pub px: u16,
    pub py: u16,
    pub sx: u16,
    pub sy: u16,
    pub lines: Option<BoxLines>,
    pub rules: CloseRules,
    pub title: String,
    pub style: Option<String>,
    pub border_style: Option<String>,
    pub command: Command,
    pub cwd: PathBuf,
    pub env: Vec<String>,
    /// The waiting command (`pd->item`).
    pub step: Option<StepId>,
    /// The waiting command's client (`cmdq_get_client(item)`).
    pub caller: Option<ClientId>,
}

/// One client's popup (`struct popup_data`).
pub struct Popup {
    /// The size and position it was opened with (`ppx`, `ppy`, `psx`,
    /// `psy`): a client resize fits them to the new terminal again.
    pub(crate) preferred: (u16, u16, u16, u16),
    pub px: u16,
    pub py: u16,
    pub sx: u16,
    pub sy: u16,
    pub lines: BoxLines,
    pub rules: CloseRules,
    pub title: String,
    /// `-s` and `-S`, applied over the options at every draw.
    pub style: Option<String>,
    pub border_style: Option<String>,
    pub term: PaneTerm,
    /// A full redraw has drawn the popup since it opened (see
    /// `PopupDraw::redrawn`).
    pub redrawn: bool,
    /// The PTY until the job closes it.
    io: Option<PaneIo>,
    /// The job's process until it is reaped.
    pid: Option<i32>,
    /// `pd->job != NULL`: the job has neither closed its output nor been
    /// reaped (`job_check_died` / `job_error_callback` both ran).
    running: bool,
    /// The job's exit (`pd->status`), `128 + SIGHUP` until it has one.
    status: i32,
    step: Option<StepId>,
    caller: Option<ClientId>,
}

impl std::fmt::Debug for Popup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Popup")
            .field("at", &(self.px, self.py, self.sx, self.sy))
            .field("lines", &self.lines)
            .field("rules", &self.rules)
            .field("pid", &self.pid)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl Popup {
    /// The job's terminal size: inside the box, or the whole popup with no
    /// border.
    #[must_use]
    pub fn inner(&self) -> (u16, u16) {
        inner(self.lines, self.sx, self.sy)
    }

    /// `popup_mode_cb`: the cursor on the client's screen.
    #[must_use]
    pub fn cursor(&self) -> (u16, u16) {
        let (cx, cy) = self.term.cursor();
        let off = u16::from(self.lines != BoxLines::None);
        (self.px + off + cx, self.py + off + cy)
    }
}

fn inner(lines: BoxLines, sx: u16, sy: u16) -> (u16, u16) {
    if lines == BoxLines::None {
        (sx, sy)
    } else {
        (sx.saturating_sub(2), sy.saturating_sub(2))
    }
}

/// `style_parse` of `-s`/`-S` over `base`, keeping only the colours
/// (popup.c:107-133).
fn colours_over(base: &mut CellStyle, extra: Option<&str>) {
    let Some(extra) = extra else { return };
    let default = CellStyle::default();
    let mut sy = Style::new(&default);
    if sy.parse(base, extra).is_ok() {
        base.fg = sy.cell.fg;
        base.bg = sy.cell.bg;
    }
}

impl Server {
    /// `popup_present`.
    pub(crate) fn popup_present(&self, c: ClientId) -> bool {
        self.clients.get(&c).is_some_and(|c| c.popup.is_some())
    }

    /// `popup_display` (popup.c:561-661): `false` when the popup is too
    /// small for its border or larger than the client, or the job could
    /// not start; nothing is shown then.
    pub(crate) fn popup_display(&mut self, c: ClientId, open: Open) -> bool {
        let tree = TreeId::Session(open.session);
        // popup.c:577-578: the session's current window's option.
        let lines = open.lines.unwrap_or_else(|| {
            let wtree = self
                .model
                .current_of(open.session)
                .map_or(TreeId::GlobalWindow, |(_, w, _)| TreeId::Window(w));
            BoxLines::from_choice(self.model.number_option(wtree, "popup-border-lines"))
        });
        let min = if lines == BoxLines::None { 1 } else { 3 };
        if open.sx < min || open.sy < min {
            return false;
        }
        let Some((cols, rows)) = self.tty_size(c) else {
            return false;
        };
        if cols < open.sx || rows < open.sy {
            return false;
        }
        let (jx, jy) = inner(lines, open.sx, open.sy);
        let shell = self.default_shell(tree);
        let mut env = self.job_environ(Some(open.session));
        for e in &open.env {
            if let Some((k, v)) = e.split_once('=') {
                env.retain(|(name, _)| name != k);
                env.push((k.to_owned(), v.to_owned()));
            }
        }
        let program = match open.command {
            Command::Shell(cmd) => {
                // job.c:204-205: JOB_DEFAULTSHELL sets SHELL for `-c`.
                env.retain(|(k, _)| k != "SHELL");
                env.push(("SHELL".into(), shell.to_string_lossy().into_owned()));
                sys::pty::Program::Command(cmd)
            }
            Command::Argv(argv) => sys::pty::Program::Argv(argv),
        };
        let Ok(spawned) = sys::pty::spawn(&sys::pty::SpawnSpec {
            program,
            shell,
            cwd: open.cwd,
            home: crate::server::find_home(&self.global_env),
            env,
            cols: jx.max(1),
            rows: jy.max(1),
        }) else {
            return false;
        };
        let io = PaneIo::new(spawned.master);
        if io
            .register(self.poll.registry(), Token(POPUP_BASE + c.0))
            .is_err()
        {
            return false;
        }
        // screen_set_default_cursor and no history: a popup keeps none.
        let term = PaneTerm::new(jx.max(1), jy.max(1), 0);
        let popup = Popup {
            preferred: (open.px, open.py, open.sx, open.sy),
            px: open.px,
            py: open.py,
            sx: open.sx,
            sy: open.sy,
            lines,
            rules: open.rules,
            title: open.title,
            style: open.style,
            border_style: open.border_style,
            term,
            redrawn: false,
            io: Some(io),
            pid: Some(spawned.pid.as_raw_nonzero().get()),
            running: true,
            status: 128 + 1,
            step: open.step,
            caller: open.caller,
        };
        // server_client_set_overlay (server-client.c:86-87) clears only an
        // earlier overlay; an open prompt stays and keeps the cursor.
        let Some(client) = self.clients.get_mut(&c) else {
            return false;
        };
        client.popup = Some(popup);
        self.update_focus(self.client_window(c));
        self.redraw_client(c);
        // This draw is the first one: the empty popup.
        if let Some(p) = self.clients.get_mut(&c).and_then(|x| x.popup.as_mut()) {
            p.redrawn = false;
        }
        true
    }

    /// `popup_modify` (popup.c:512-558): new title, styles, lines and
    /// close rules for the open popup; no new job.
    pub(crate) fn popup_modify(
        &mut self,
        c: ClientId,
        title: String,
        style: Option<String>,
        border_style: Option<String>,
        lines: Option<BoxLines>,
        rules: Option<CloseRules>,
    ) {
        let Some(p) = self.clients.get_mut(&c).and_then(|x| x.popup.as_mut()) else {
            return;
        };
        p.title = title;
        if style.is_some() {
            p.style = style;
        }
        if border_style.is_some() {
            p.border_style = border_style;
        }
        if let Some(lines) = lines {
            // Only a change to or from no border resizes the terminal
            // (popup.c:541-550).
            let resize = (lines == BoxLines::None) != (p.lines == BoxLines::None);
            p.lines = lines;
            if resize {
                let (jx, jy) = p.inner();
                p.term.resize(jx.max(1), jy.max(1));
                if let Some(io) = &p.io {
                    let _ = io.resize(jx.max(1), jy.max(1));
                }
            }
        }
        if let Some(rules) = rules {
            p.rules = rules;
        }
        self.redraw_client(c);
    }

    /// `popup_resize_cb` (popup.c:285-322): the client's terminal changed
    /// size. The popup keeps its preferred rectangle where it fits, else is
    /// clipped to the terminal and moved back inside; its terminal and job
    /// follow, unless a bordered popup shrank to 2 cells or fewer.
    pub(crate) fn popup_resize(&mut self, c: ClientId) {
        let Some((cols, rows)) = self.tty_size(c) else {
            return;
        };
        let Some(p) = self.clients.get_mut(&c).and_then(|x| x.popup.as_mut()) else {
            return;
        };
        let (ppx, ppy, psx, psy) = p.preferred;
        p.sy = psy.min(rows);
        p.sx = psx.min(cols);
        p.py = if ppy + p.sy > rows { rows - p.sy } else { ppy };
        p.px = if ppx + p.sx > cols { cols - p.sx } else { ppx };
        if p.lines == BoxLines::None || (p.sx > 2 && p.sy > 2) {
            let (jx, jy) = p.inner();
            p.term.resize(jx.max(1), jy.max(1));
            if let Some(io) = &p.io {
                let _ = io.resize(jx.max(1), jy.max(1));
            }
        }
    }

    /// `server_client_clear_overlay` for a popup (`popup_free_cb`): the job
    /// is killed (`job_free`), the waiting command carries on, a command
    /// client with the popup's status as its exit code.
    pub(crate) fn popup_close(&mut self, c: ClientId) {
        let Some(mut popup) = self.clients.get_mut(&c).and_then(|x| x.popup.take()) else {
            return;
        };
        if let Some(io) = popup.io.take() {
            io.deregister(self.poll.registry());
        }
        if popup.running
            && let Some(pid) = popup.pid.and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
        if popup.pid.is_some() {
            // Reaped by reap_children; nobody waits for it any more.
            self.orphans.extend(popup.pid);
        }
        if let Some(step) = popup.step {
            let status = popup.status;
            let caller = popup.caller;
            self.cmdq_with_item(step, |server, item| {
                if caller.is_some_and(|c| server.client_session(c).is_none()) {
                    item.status = status;
                }
            });
            self.cmdq_continue(step);
        }
        // server_client_clear_overlay: focus back, then a full redraw.
        self.update_focus(self.client_window(c));
        self.redraw_client(c);
    }

    /// The popup PTY of client `c` is readable: feed the terminal
    /// (`popup_job_update_cb`); at EOF the output is closed.
    pub(crate) fn popup_readable(&mut self, c: ClientId) {
        let Some(p) = self.clients.get_mut(&c).and_then(|x| x.popup.as_mut()) else {
            return;
        };
        let mut closed = false;
        while let Some(io) = p.io.as_mut() {
            match io.read() {
                ReadResult::Data(bytes) => p.term.feed(&bytes),
                ReadResult::WouldBlock => break,
                ReadResult::Closed => {
                    closed = true;
                    break;
                }
            }
        }
        // A popup's passthrough is not forwarded (vtmux has no `-E` use for it).
        let _ = p.term.take_passthrough();
        let replies = p.term.take_replies();
        if let Some(io) = p.io.as_mut().filter(|_| !replies.is_empty()) {
            io.write(&replies);
        }
        if closed {
            if let Some(io) = p.io.take() {
                io.deregister(self.poll.registry());
            }
            self.popup_check_done(c);
        }
    }

    /// The popup PTY can take more input.
    pub(crate) fn popup_writable(&mut self, c: ClientId) {
        if let Some(io) = self
            .clients
            .get_mut(&c)
            .and_then(|x| x.popup.as_mut())
            .and_then(|p| p.io.as_mut())
        {
            io.flush();
        }
        self.popup_reregister(c);
    }

    fn popup_reregister(&mut self, c: ClientId) {
        if let Some(io) = self
            .clients
            .get(&c)
            .and_then(|x| x.popup.as_ref())
            .and_then(|p| p.io.as_ref())
        {
            let _ = io.reregister(self.poll.registry(), Token(POPUP_BASE + c.0));
        }
    }

    /// `job_check_died` for a popup's process: `true` when `pid` was one
    /// (an open popup's, or a closed one's not yet reaped).
    pub(crate) fn popup_died(&mut self, pid: i32, status: JobStatus) -> bool {
        if let Some(i) = self.orphans.iter().position(|&p| p == pid) {
            self.orphans.swap_remove(i);
            return true;
        }
        let Some(c) = self
            .clients
            .iter()
            .find(|(_, x)| x.popup.as_ref().is_some_and(|p| p.pid == Some(pid)))
            .map(|(&id, _)| id)
        else {
            return false;
        };
        // Drain what the job wrote before it went.
        self.popup_readable(c);
        if let Some(p) = self.clients.get_mut(&c).and_then(|x| x.popup.as_mut()) {
            p.pid = None;
            // popup_job_complete_cb (popup.c:489-495): the exit code, or
            // the signal number.
            p.status = match status {
                JobStatus::Exited(n) | JobStatus::Signaled(n) => n,
            };
        }
        self.popup_check_done(c);
        true
    }

    /// Both the PTY has closed and the process has been reaped: the job is
    /// complete (`popup_job_complete_cb`), and `-E`/`-EE` may close it.
    fn popup_check_done(&mut self, c: ClientId) {
        let Some(p) = self.clients.get_mut(&c).and_then(|x| x.popup.as_mut()) else {
            return;
        };
        if !p.running || p.io.is_some() || p.pid.is_some() {
            return;
        }
        p.running = false;
        if p.rules.on_exit || (p.rules.on_exit_zero && p.status == 0) {
            self.popup_close(c);
        }
    }

    /// `popup_key_cb` (popup.c:430-460) for a key from client `c`: `true`
    /// when the popup took it (it always does while up).
    pub(crate) fn popup_key(&mut self, c: ClientId, key: Key) -> bool {
        let Some(p) = self.clients.get_mut(&c).and_then(|x| x.popup.as_mut()) else {
            return false;
        };
        let cancel = key == Key::ascii(0x1b) || key == Key::ascii(b'c').ctrl();
        let closes_on_exit = p.rules.on_exit || p.rules.on_exit_zero;
        let paste = key == Key::PASTE_START || key == Key::PASTE_END;
        if (!closes_on_exit || !p.running) && cancel || !p.running && p.rules.any_key && !paste {
            self.popup_close(c);
            return true;
        }
        if p.running
            && let Some(k) = crate::keys::Key::from_core(key)
            && let Some(io) = p.io.as_mut()
        {
            // input_key: in the popup terminal's own modes.
            let modes = crate::keys::KeyModes {
                app_cursor: p.term.app_cursor(),
                app_keypad: p.term.app_keypad(),
            };
            io.write(&crate::keys::encode(&k, modes));
            self.popup_reregister(c);
        }
        true
    }

    /// The client's terminal size.
    pub(crate) fn tty_size(&self, c: ClientId) -> Option<(u16, u16)> {
        let tty = self.clients.get(&c)?.hello.as_ref()?.tty.as_ref()?;
        Some((
            u16::try_from(tty.cols).unwrap_or(u16::MAX),
            u16::try_from(tty.rows).unwrap_or(u16::MAX),
        ))
    }

    /// `popup_reapply_styles` (popup.c:96-135): `popup-style` and
    /// `popup-border-style` of the client's current window, expanded as
    /// formats at every draw, then `-s`/`-S` over their colours.
    pub(crate) fn popup_styles(
        &self,
        c: ClientId,
        source: &crate::render::window::Source<'_>,
    ) -> Option<(CellStyle, CellStyle)> {
        let client = self.clients.get(&c)?;
        let p = client.popup.as_ref()?;
        let s = client.attached?;
        let (_, w, _) = self.model.current_of(s)?;
        let ctx = self.format_ctx(crate::target::Found::from_session(&self.model, s), Some(c));
        let tree = TreeId::Window(w);
        let mut defaults = source.style(tree, "popup-style", &ctx);
        colours_over(&mut defaults, p.style.as_deref());
        let mut border = source.style(tree, "popup-border-style", &ctx);
        colours_over(&mut border, p.border_style.as_deref());
        Some((
            CellStyle {
                fg: defaults.fg,
                bg: defaults.bg,
                ..CellStyle::default()
            },
            CellStyle {
                fg: border.fg,
                bg: border.bg,
                ..CellStyle::default()
            },
        ))
    }
}
