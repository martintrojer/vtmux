//! Restored panes (vtmux only; live-upgrade spec, decisions 31 and 32,
//! *Respawning a restored pane*). A cold restore marks each pane that ran
//! in the checkpoint with the checkpoint's write time
//! (`Pane::restored_from`, `server::restore::prepare`). Until it is
//! respawned the renderer draws its status over its bottom row, never in
//! its grid; respawning it keeps its screen above a `restored <time>` row
//! instead of clearing it as tmux's `screen_reinit` does; Enter from a
//! client respawns it, and `vtmux-persist-respawn auto` respawns the
//! restored shells once the startup config has run. tmux has no restored
//! panes: every other pane behaves as in tmux whatever the option says.

use std::io;
use std::time::Instant;

use vtmux_core::ids::{PaneId, SessionId};
use vtmux_core::keys::Key;
use vtmux_core::options::TreeId;
use vtmux_core::style::CellStyle;
use vtmux_core::styled;
use vtmux_vt::{CaptureOpts, Cell, ExtendedKeys, LineRef, PaneTerm, Row, Style};

use crate::model::Pane;
use crate::server::{ClientId, PaneSpawn, Server, spawn_failed};
use crate::target::Found;

/// How long the program a restored pane was respawned into must run for
/// its exit to be an ordinary one (decision 31, *A restart that does not
/// run*): a missing binary or shell, or a command that fails at once,
/// exits well within it.
pub(crate) const RESTART_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// `vtmux-persist-respawn`'s values (decision 32).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Respawn {
    Off,
    Key,
    Auto,
}

/// Whether `vtmux-persist-respawn auto`'s pass is still to run on this
/// server: only one a cold restore built, once its config has run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum AutoPass {
    #[default]
    Done,
    Due,
}

impl Server {
    /// `vtmux-persist-respawn` (decision 32).
    pub(crate) fn respawn_setting(&self) -> Respawn {
        match self
            .model
            .number_option(TreeId::Server, "vtmux-persist-respawn")
        {
            0 => Respawn::Off,
            2 => Respawn::Auto,
            _ => Respawn::Key,
        }
    }

    /// A restored pane (decision 31) still waiting for its respawn: dead,
    /// with the checkpoint time it came from.
    pub(crate) fn waits_restored(&self, p: PaneId) -> Option<i64> {
        let pane = self.model.panes.get(&p)?;
        pane.restored_from
            .filter(|_| pane.exited && pane.io.is_none())
    }

    /// Pane `p`'s program went and its status is in (`server_destroy_pane`
    /// is about to decide): true when the pane stays as it is, whatever
    /// `remain-on-exit` says (decision 31, *A restart that does not run*).
    /// A pane still waiting as restored stays; one respawned from a
    /// restored pane whose program ran less than [`RESTART_GRACE`] goes
    /// back to waiting: dead, restored from the same checkpoint, its
    /// screen (the old one, the marker, what the program wrote) kept,
    /// `pane-died` fired as for a pane `remain-on-exit` keeps, no dead
    /// line written (the hint says the restart failed).
    pub(crate) fn keep_restored(&mut self, p: PaneId) -> bool {
        let Some(pane) = self.model.panes.get_mut(&p) else {
            return false;
        };
        if pane.restored_from.is_some() {
            return true;
        }
        let Some((from, at)) = pane.restart_guard.take() else {
            return false;
        };
        if at.elapsed() >= RESTART_GRACE {
            return false;
        }
        pane.restored_from = Some(from);
        pane.dead_time = Some(crate::model::now_epoch());
        self.fire_pane_exit("pane-died", p);
        self.hide_restored_cursor(p);
        self.model.redraw_panes.insert(p);
        true
    }

    /// A restored pane's cursor is hidden as a dead pane's is
    /// (server-fn.c:417, `MODE_CURSOR` off): a mode, not grid content.
    pub(crate) fn hide_restored_cursor(&mut self, p: PaneId) {
        if let Some(pane) = self.model.panes.get_mut(&p) {
            pane.term.reset_input();
            pane.term.feed(b"\x1b[?25l");
            drop(pane.term.take_replies());
        }
    }

    /// The status text over a restored pane's bottom row (*Respawning a
    /// restored pane*, *The hint*): with `key` or `auto`,
    /// `Restored <time>: Enter restarts <command>`; with `off`,
    /// `remain-on-exit-format`'s dead line, drawn as tmux draws it
    /// (`format_draw` on the default cell), and none for an empty format
    /// (server-fn.c `server_destroy_pane` draws nothing for it). Every
    /// column is the line's. After a restart that did not run (its status
    /// known), `Restored <time>: restart failed (status N); Enter restarts
    /// <command>`.
    fn restored_hint(&self, p: PaneId, from: i64) -> Option<Vec<styled::Cell>> {
        let pane = &self.model.panes[&p];
        let sx = usize::from(pane.term.cols());
        let text = if self.respawn_setting() == Respawn::Off {
            let template = self
                .model
                .options
                .get(TreeId::Pane(p), "remain-on-exit-format")
                .map(|e| e.value.to_text(e.def, None, false))
                .unwrap_or_default();
            if template.is_empty() {
                return None;
            }
            let found = Found {
                window: Some(pane.window),
                pane: Some(p),
                ..Found::default()
            };
            let ctx = self.format_ctx(found, None);
            vtmux_core::format::expand_str(&template, &ctx)
        } else {
            let command = crate::names::stringify_argv(&pane.argv);
            let command = if command.is_empty() {
                pane.shell.to_string_lossy().into_owned()
            } else {
                command
            };
            let failed = match pane.status {
                Some(crate::jobs::JobStatus::Exited(n)) => format!("restart failed (status {n}); "),
                Some(crate::jobs::JobStatus::Signaled(n)) => {
                    format!("restart failed (signal {n}); ")
                }
                _ => String::new(),
            };
            // `#[...]` in a command is text, not a style.
            format!(
                "Restored {}: {failed}Enter restarts {}",
                self.restored_time(from),
                command.replace('#', "##")
            )
        };
        let under = vec![
            styled::Cell {
                text: " ".into(),
                width: 1,
                style: CellStyle::default(),
            };
            sx
        ];
        Some(styled::draw_onto(&CellStyle::default(), under, &text, false, false).cells)
    }

    /// A checkpoint's write time as the hint and the marker show it, in
    /// the server's zone.
    pub(crate) fn restored_time(&self, from: i64) -> String {
        let live = &self.model.live;
        vtmux_core::time::WallClock::local_time(live, from)
            .unwrap_or_else(|| vtmux_core::time::Tm::utc(from))
            .strftime("%Y-%m-%d %H:%M")
    }

    /// Before a render: each restored pane waiting for its respawn has its
    /// hint as the option, its width and its format say now, and a pane
    /// whose hint changed is drawn again; a pane no longer waiting has
    /// none.
    pub(crate) fn refresh_restored_hints(&mut self) {
        let panes: Vec<PaneId> = self
            .model
            .panes
            .iter()
            .filter(|(_, x)| x.restored_from.is_some() || x.restored_hint.is_some())
            .map(|(&p, _)| p)
            .collect();
        for p in panes {
            let hint = self.waits_restored(p).and_then(|from| {
                let cols = self.model.panes[&p].term.cols();
                Some((cols, self.restored_hint(p, from)?))
            });
            let pane = self.model.panes.get_mut(&p).expect("listed above");
            if pane.restored_hint != hint {
                pane.restored_hint = hint;
                self.model.redraw_panes.insert(p);
            }
        }
    }

    /// Enter from client `id`, after the key tables found no binding
    /// (decision 32, *The key*): a restored pane that is the client's
    /// active pane, in no mode, respawns, and the key is taken: it goes to
    /// no program and no synchronized peer. A read-only client's key is
    /// not one.
    pub(crate) fn restored_enter(&mut self, id: ClientId, key: Key) -> bool {
        if key.without_flags() != Key::ascii(b'\r') || self.respawn_setting() == Respawn::Off {
            return false;
        }
        let Some(c) = self.clients.get(&id).filter(|c| !c.flags.read_only) else {
            return false;
        };
        let Some(s) = c.attached else { return false };
        let Some((index, _, p)) = self.model.current_of(s) else {
            return false;
        };
        if self.waits_restored(p).is_none() || !self.model.panes[&p].modes.is_empty() {
            return false;
        }
        if let Err(why) = self.respawn_restored(s, index, p) {
            self.add_message(why);
        }
        true
    }

    /// Respawn restored pane `p` (in session `s`, window index `index`) as
    /// `respawn-pane -t` it would be: its saved command, shell and cwd,
    /// the session's environment (environ.c `environ_for_session`), no
    /// client's; `pane-created` fires as for `respawn-pane`. The error is
    /// `respawn-pane`'s text.
    pub(crate) fn respawn_restored(
        &mut self,
        s: SessionId,
        index: u32,
        p: PaneId,
    ) -> Result<(), String> {
        let pane = &self.model.panes[&p];
        let (w, argv, cwd, shell, rect) = (
            pane.window,
            pane.argv.clone(),
            pane.cwd.clone(),
            pane.shell.clone(),
            pane.rect,
        );
        let session_env = self.model.sessions[&s].env.clone();
        let chars = self.model.sessions[&s].control_chars.clone();
        let spawned = self.respawn_pane(&PaneSpawn {
            pane: p,
            window: w,
            session: s,
            session_env: &session_env,
            control_chars: chars.as_deref(),
            extra_env: &[],
            client_path: None,
            argv,
            cwd,
            rect,
            cell_pixels: self.model.windows[&w].cell_pixels,
            shell: Some(shell),
            empty: false,
        });
        self.model.redraw_panes.insert(p);
        match spawned {
            Ok(()) => {
                self.fire_pane_created(s, index, p, false, true);
                Ok(())
            }
            Err(e) => Err(format!("respawn pane failed: {p}: {}", spawn_failed(&e))),
        }
    }

    /// `vtmux-persist-respawn auto`'s one pass after a cold restore, once
    /// the config has run (decision 32, *auto*): each pane once, in
    /// session (name), window (index) and pane order, respawned when it is
    /// still restored and dead and its saved command is empty (it ran the
    /// saved shell), in the first session holding its window. A pane that
    /// fails to spawn stays restored, named in `show-messages`.
    pub(crate) fn auto_respawn_restored(&mut self) {
        if self.respawn_setting() != Respawn::Auto {
            return;
        }
        let mut seen = std::collections::HashSet::new();
        let mut todo = Vec::new();
        for &s in self.model.by_name.values() {
            for (&index, w) in &self.model.sessions[&s].windows {
                let Some(window) = self.model.windows.get(w) else {
                    continue;
                };
                for &p in &window.panes.panes {
                    if seen.insert(p) {
                        todo.push((s, index, p));
                    }
                }
            }
        }
        for (s, index, p) in todo {
            let shell = self.model.panes.get(&p).is_some_and(|x| x.argv.is_empty());
            if !shell || self.waits_restored(p).is_none() || !self.model.sessions.contains_key(&s) {
                continue;
            }
            if let Err(why) = self.respawn_restored(s, index, p) {
                self.add_message(format!("vtmux-persist-respawn: {why}"));
            }
        }
    }

    /// [`Self::respawn_pane`] for a pane restored from the checkpoint
    /// written at `from` (live-upgrade spec, decision 31, *Respawning a
    /// restored pane*): the resets of a respawn without the clear and the
    /// cursor's homing, the alternate screen's rows (when it was up)
    /// appended below the main screen's last occupied row, a dim
    /// `restored <time>` row under them, the cursor at the start of the
    /// next row, then the program. A spawn that fails leaves the pane as
    /// it was, still restored, with tmux's new command and directory; `-E`
    /// stores those and writes nothing.
    pub(crate) fn respawn_restored_pane(
        &mut self,
        ps: &PaneSpawn<'_>,
        from: i64,
    ) -> io::Result<()> {
        let p = ps.pane;
        let keep = |pane: &mut Pane| {
            if !ps.argv.is_empty() {
                pane.argv.clone_from(&ps.argv);
            }
            pane.cwd.clone_from(&ps.cwd);
        };
        if ps.empty {
            if let Some(pane) = self.model.panes.get_mut(&p) {
                keep(pane);
            }
            return Ok(());
        }
        self.control_reset_pane(p);
        self.reset_mode_all(p);
        let fresh = match self.spawn_pane(ps) {
            Ok(fresh) => fresh,
            Err(e) => {
                if let Some(pane) = self.model.panes.get_mut(&p) {
                    keep(pane);
                }
                return Err(e);
            }
        };
        let marker = format!("restored {}", self.restored_time(from));
        let extended_keys = self.model.extended_keys();
        let Some(pane) = self.model.panes.get_mut(&p) else {
            return Ok(());
        };
        // As an ordinary respawn: the old program's grants and requests go.
        pane.kitty_owner = [None; 2];
        pane.reset_graphics();
        pane.requests.clear();
        keep_screen(&mut pane.term, extended_keys, &marker);
        pane.compress.observe(&pane.term, Instant::now());
        pane.exited = false;
        pane.status = None;
        pane.dead_time = None;
        pane.changed = true;
        pane.restored_from = None;
        pane.restart_guard = Some((from, Instant::now()));
        pane.io = fresh.io;
        pane.empty = fresh.empty;
        pane.last_rename = fresh.last_rename;
        pane.pid = fresh.pid;
        pane.tty = fresh.tty;
        pane.argv = fresh.argv;
        pane.shell = fresh.shell;
        pane.cwd = fresh.cwd;
        Ok(())
    }
}

/// A restored pane's terminal made ready for its new program, in the
/// order *Respawning a restored pane* gives: the alternate screen's
/// visible rows taken when it is up; an ordinary respawn's resets without
/// the clear and the cursor's homing (leaving the alternate screen
/// resizes the main one to the pane, as tmux's `screen_alternate_off`);
/// the rows appended below the main screen's last occupied row; `marker`
/// dim, hard and clipped to the width on the row below them (the top row
/// of a blank screen); the cursor at the start of the next row, a line
/// feed scrolling at the bottom.
fn keep_screen(term: &mut PaneTerm, extended_keys: ExtendedKeys, marker: &str) {
    // The alternate screen's visible rows as `capture-pane -e -J` gives
    // them: styles, charsets and links, a wrapped row joined to the next
    // so the main screen (the same width) wraps it again.
    let alternate: Option<Vec<u8>> = term
        .alternate_on()
        .then(|| {
            let h = term.history_size();
            let used = term
                .rows_in(h..h + usize::from(term.rows()))
                .collect::<Vec<_>>()
                .iter()
                .rposition(row_occupied)?;
            let captured = vtmux_vt::capture(
                term,
                CaptureOpts {
                    escapes: true,
                    join: true,
                    start: LineRef::Line(0),
                    end: LineRef::Line(i64::try_from(used).ok()?),
                    ..CaptureOpts::default()
                },
            );
            let captured = captured.strip_suffix(b"\n").unwrap_or(&captured);
            let mut out = Vec::with_capacity(captured.len() + 16);
            for (i, line) in captured.split(|&b| b == b'\n').enumerate() {
                if i > 0 {
                    out.extend_from_slice(b"\r\n");
                }
                out.extend_from_slice(line);
            }
            out.extend_from_slice(b"\x1b[m\x0f\x1b]8;;\x1b\\");
            Some(out)
        })
        .flatten();
    term.reset_input();
    term.stop_sync();
    term.reset_kitty_keys();
    term.set_scroll_on_clear(false);
    term.end_inband_resize();
    // `Server::respawn_pane`'s resets but for the clear (`\x1b[2J`) and
    // the homing (`\x1b[H`), with the charsets, the link and the left and
    // right margins reset for the rows written next.
    term.feed(
        b"\x1b[r\x1b[?6l\x1b[?47l\x1b[m\x0f\x1b(B\x1b]8;;\x1b\\\x1b[?69l\x1b[?7h\x1b[?25h\x1b[?1l\x1b>\x1b[4l\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1004l\x1b[?2031l",
    );
    let _ = term.take_replies();
    term.reset_tab_stops();
    term.reinit_cursor_modes();
    term.forget_alternate_saved();
    term.reset_progress_bar();
    term.set_extended_keys(extended_keys);
    term.reset_key_mode();
    let h = term.history_size();
    let last = term
        .rows_in(h..h + usize::from(term.rows()))
        .collect::<Vec<_>>()
        .iter()
        .rposition(row_occupied);
    // From the last occupied row each line starts with a line feed; on a
    // blank screen the first goes on the top row.
    let mut first = last.is_none();
    let mut bytes = format!("\x1b[{};1H", last.map_or(1, |y| y + 1)).into_bytes();
    let cols = usize::from(term.cols());
    // ASCII: one column a character.
    let marker: String = marker.chars().take(cols).collect();
    let mut marker_row = b"\x1b[2m".to_vec();
    marker_row.extend_from_slice(marker.as_bytes());
    marker_row.extend_from_slice(b"\x1b[m");
    for line in alternate.iter().chain(std::iter::once(&marker_row)) {
        if !std::mem::take(&mut first) {
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(line);
    }
    bytes.extend_from_slice(b"\r\n");
    term.feed(&bytes);
    let _ = term.take_replies();
    drop(term.take_passthrough());
    term.take_events();
}

/// Whether a row of a restored pane's screen was written (decision 31): a
/// cell with text (an explicit space too), a style, a link, a tab or the
/// line-drawing set, or the row wraps.
fn row_occupied(row: &Row) -> bool {
    row.wrapped || row.cells.iter().any(cell_occupied)
}

fn cell_occupied(c: &Cell) -> bool {
    !c.text.is_empty() || c.style != Style::default() || c.link.is_some() || c.acs || c.tab
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `keep_screen` leaves of a 10x4 screen fed `bytes`: its rows
    /// and the cursor.
    fn kept(bytes: &[u8]) -> (Vec<String>, (u16, u16)) {
        let mut t = PaneTerm::new(10, 4, 100);
        t.feed(bytes);
        keep_screen(&mut t, ExtendedKeys::Off, "restored 12:00");
        let h = t.history_size();
        let rows = t.rows_in(0..h + 4).map(|r| r.text()).collect();
        (rows, t.cursor())
    }

    /// A row is occupied when something was written in it: an explicit
    /// space or a styled erase counts, an untouched row does not; a blank
    /// screen takes the marker on its top row.
    #[test]
    fn the_marker_goes_under_the_last_written_row() {
        let rows = |r: &[&str]| r.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(kept(b""), (rows(&["restored 1", "", "", ""]), (0, 1)));
        assert_eq!(
            kept(b"a\r\n \r\n"),
            (rows(&["a", "", "restored 1", ""]), (0, 3))
        );
        assert_eq!(
            kept(b"a\r\n\r\n\x1b[41m\x1b[K\x1b[m"),
            (rows(&["a", "", "", "restored 1", ""]), (0, 3))
        );
        // Text past the cursor counts, the cursor's place does not; the
        // cursor's row after a marker on the last row scrolls.
        assert_eq!(
            kept(b"\x1b[3;1Hx\x1b[H"),
            (rows(&["", "", "x", "restored 1", ""]), (0, 3))
        );
    }

    /// The marker is clipped to the width and hard; the alternate screen's
    /// rows (trailing blank rows left out) go below the main screen's.
    #[test]
    fn an_alternate_screen_is_appended_and_the_marker_clipped() {
        let mut t = PaneTerm::new(10, 4, 100);
        t.feed(b"$ vi\r\n\x1b[?1049h\x1b[H~ x");
        keep_screen(&mut t, ExtendedKeys::Off, "restored 2026-11-11 09:00");
        assert!(!t.alternate_on());
        let h = t.history_size();
        let rows: Vec<String> = t.rows_in(0..h + 4).map(|r| r.text()).collect();
        assert_eq!(rows, ["$ vi", "~ x", "restored 2", ""]);
        assert!(!t.row(h + 2).wrapped && t.row(h + 2).cells[0].style.faint);
        assert_eq!(t.cursor(), (0, 3));
    }

    /// The alternate screen's rows keep what `capture-pane -J` and `-e`
    /// read of them (cmd-capture-pane.c `cmd_capture_pane_history`): a
    /// soft wrap (`GRID_LINE_WRAPPED`) and a hyperlink with its id.
    #[test]
    fn an_alternate_screen_keeps_its_wraps_and_links() {
        let mut t = PaneTerm::new(10, 5, 100);
        t.feed(
            b"$ vi\r\n\x1b[?1049h\x1b[Habcdefghij\x1b]8;id=x;https://example.com/\x1b\\ABCDEFGHIJ\x1b]8;;\x1b\\",
        );
        keep_screen(&mut t, ExtendedKeys::Off, "restored 2026-11-11 09:00");
        let rows: Vec<String> = t.rows_in(0..5).map(|r| r.text()).collect();
        assert_eq!(rows, ["$ vi", "abcdefghij", "ABCDEFGHIJ", "restored 2", ""]);
        assert!(t.row(1).wrapped && !t.row(2).wrapped);
        assert_eq!(t.hyperlink(0, 2).as_deref(), Some("https://example.com/"));
        assert_eq!(t.hyperlink(0, 1), None);
        assert_eq!(t.hyperlink(0, 3), None, "the marker has no link");
        let all = vtmux_vt::capture(
            &t,
            vtmux_vt::CaptureOpts {
                escapes: true,
                join: true,
                ..vtmux_vt::CaptureOpts::default()
            },
        );
        let all = String::from_utf8_lossy(&all);
        assert!(
            all.contains("abcdefghij\x1b]8;id=x;https://example.com/\x1b\\ABCDEFGHIJ"),
            "{all:?}"
        );
    }
}
