//! View mode (`window_view_mode`, window-copy.c:192-203): copy mode over a
//! grid of its own that command output is appended to, rather than a copy
//! of the pane. `cmdq_print` from an attached client and `run-shell` output
//! to a pane land here (`server_client_print`, `cmd_run_shell_print`).
//!
//! tmux writes each line into the mode's backing screen through an input
//! parser (`window_copy_vadd`, window-copy.c:715-760), so escapes in the
//! text are interpreted. Here the mode keeps a private terminal the text
//! is fed to, and the copy's grid is read again from it after each line.
//! Everything else (keys, movement, search, selection, drawing) is
//! [`CopyMode`]'s.

use vtmux_core::ids::PaneId;
use vtmux_vt::PaneTerm;

use super::{CopyMode, Mode, screen_key_mode};
use crate::server::Server;

/// The backing screen's history limit (`screen_init(..., UINT_MAX)`):
/// unlimited in practice, with room for the terminal's own arithmetic.
const HISTORY: usize = usize::MAX / 4;

/// One view mode (`window_copy_mode_data` with `viewmode` set).
#[derive(Debug)]
pub struct ViewMode {
    pub copy: CopyMode,
    /// `data->backing` as written, with `data->ictx`'s parser state.
    term: PaneTerm,
    /// `data->backing_written`: a line was added, so the next starts on a
    /// new line.
    written: bool,
    /// A resize reflowed the copy's grid by tmux's rules and the terminal
    /// by Ghostty's: the next line reads the whole grid again.
    stale: bool,
}

impl ViewMode {
    /// `window_copy_view_init` (window-copy.c:652-674): an empty `sx` x
    /// `sy` backing, the cursor at the top left, no line numbers. `vi` is
    /// `mode-keys`; `pane_search` the pane's last search.
    #[must_use]
    pub fn new((sx, sy): (usize, usize), vi: bool, pane_search: Option<(String, bool)>) -> Self {
        let clamp = |n: usize| u16::try_from(n.max(1)).unwrap_or(u16::MAX);
        let term = PaneTerm::new(clamp(sx), clamp(sy), HISTORY);
        let copy = CopyMode::view(&term, (sx, sy), vi, pane_search);
        Self {
            copy,
            term,
            written: false,
            stale: false,
        }
    }

    /// `window_copy_add` with `parse` set: `text` on a new line (CRLF
    /// before every line but the first), escapes interpreted. The view
    /// keeps its place: history the line pushed out is added to the
    /// scroll offset.
    pub fn add(&mut self, text: &[u8]) {
        let from = (!self.stale).then(|| self.term.history_size());
        let mut bytes = Vec::with_capacity(text.len() + 2);
        if self.written {
            bytes.extend_from_slice(b"\r\n");
        }
        self.written = true;
        bytes.extend_from_slice(text);
        self.term.feed(&bytes);
        // window_copy_init_ctx_cb: nothing goes anywhere from the backing.
        drop(self.term.take_replies());
        drop(self.term.take_passthrough());
        self.term.take_bells();
        self.copy.view_added(&self.term, from);
        self.stale = false;
    }

    /// `window_copy_resize`: the copy follows copy mode's rules and the
    /// backing is resized with it, `screen_resize_cursor(..., 1, 0, 0)`
    /// (screen.c:354-400): a shorter screen eats no blank lines (Ghostty
    /// drops the ones below its cursor, so the cursor goes to the bottom
    /// first), and the cursor keeps its line unless that went into history
    /// (then the top left). A reflow does not keep it at all
    /// (`screen_reflow` without `cursor`, screen.c:681-684): the top left
    /// of the screen, so the next line overwrites the screen's second row.
    pub fn resize(&mut self, sx: usize, sy: usize) {
        self.copy.resize(sx, sy);
        let clamp = |n: usize| u16::try_from(n.max(1)).unwrap_or(u16::MAX);
        let (cols, rows) = (clamp(sx), clamp(sy));
        let (old_cols, old_rows) = (self.term.cols(), self.term.rows());
        if (cols, rows) == (old_cols, old_rows) {
            return;
        }
        let (cx, cy) = self.term.cursor();
        let line = self.term.history_size() + usize::from(cy);
        if rows < old_rows {
            self.term.feed(format!("\x1b[{old_rows};1H").as_bytes());
        }
        self.term.resize(cols, rows);
        let hsize = self.term.history_size();
        let at = if cols != old_cols || line < hsize {
            "\x1b[H".to_owned()
        } else {
            format!("\x1b[{};{}H", line - hsize + 1, cx + 1)
        };
        self.term.feed(at.as_bytes());
        self.stale = true;
    }
}

impl Server {
    /// Text for pane `p`'s view mode, as `server_client_print` and
    /// `cmd_run_shell_print` write it: view mode is entered unless it is on
    /// top already (`window_pane_set_mode(wp, NULL, &window_view_mode, ...)`,
    /// which brings one further down to the top), then each of `lines`
    /// is added (`window_copy_add(wp, 1, ...)`) and the pane redrawn.
    pub(crate) fn view_add<'a>(&mut self, p: PaneId, lines: impl IntoIterator<Item = &'a [u8]>) {
        let is_view = |m: &Mode| matches!(m, Mode::View(_));
        if self.model.pane_may_enter(p, is_view) {
            self.reset_no_stack(p);
            let Some(pane) = self.model.panes.get(&p) else {
                return;
            };
            let size = (usize::from(pane.term.cols()), usize::from(pane.term.rows()));
            let search = pane.search.clone();
            let vi = self.model.mode_keys_vi(p);
            self.model.push_mode(
                p,
                is_view,
                |model| {
                    let mut m = ViewMode::new(size, vi, search);
                    m.copy.key_mode = screen_key_mode(model);
                    Mode::View(Box::new(m))
                },
                false,
            );
        }
        let Some(Mode::View(m)) = self
            .model
            .panes
            .get_mut(&p)
            .and_then(|x| x.modes.first_mut())
        else {
            return;
        };
        for line in lines {
            m.add(line);
        }
        self.model.redraw_panes.insert(p);
    }

    /// `server_client_print` with `parse` set for an attached terminal
    /// client (server-client.c:3260-3278): the text goes to the view mode
    /// of its session's active pane, one line per LF-ended piece, then the
    /// rest. tmux adds a NUL to non-empty text first, so the rest is never
    /// empty: a text ending in LF adds an empty line after it, an empty
    /// text adds nothing (view mode is still entered). Each line stops at a
    /// NUL (`%s`).
    pub(crate) fn view_print(&mut self, s: vtmux_core::ids::SessionId, text: &[u8]) {
        let Some(p) = crate::target::Found::from_session(&self.model, s).pane else {
            return;
        };
        let lines = text
            .split(|&b| b == b'\n')
            .filter(|_| !text.is_empty())
            .map(|l| l.split(|&b| b == 0).next().unwrap_or_default());
        self.view_add(p, lines);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(v: &ViewMode) -> Vec<String> {
        let s = v.copy.screen(false, &|_, _| {});
        (0..s.rows)
            .map(|y| {
                (0..s.cols)
                    .map(|x| s.cell(x, y).map_or(String::new(), |c| c.text.clone()))
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// Lines go one under the other from the top, escapes interpreted;
    /// the view stays at the top as they scroll into history.
    #[test]
    fn lines_append_and_the_view_stays_at_the_top() {
        let mut v = ViewMode::new((10, 3), false, None);
        v.add(b"one");
        v.add(b"\x1b[1mtwo");
        assert_eq!(lines(&v), ["one", "two", ""]);
        assert_eq!(v.copy.position(0), (0, 0));
        for i in 3..=5 {
            v.add(format!("l{i}").as_bytes());
        }
        assert_eq!(lines(&v), ["one", "two", "l3"]);
        assert_eq!(v.copy.position(0), (2, 2));
        assert_eq!(v.copy.cursor(), (0, 0));
    }

    /// An empty first line still counts: the next starts below it.
    #[test]
    fn an_empty_line_is_a_line() {
        let mut v = ViewMode::new((10, 3), false, None);
        v.add(b"");
        v.add(b"x");
        assert_eq!(lines(&v), ["", "x", ""]);
    }

    /// After a resize the copy is reflowed; the next line reads the
    /// backing again.
    #[test]
    fn resize_then_add() {
        let mut v = ViewMode::new((10, 3), false, None);
        v.add(b"abcdefgh");
        v.resize(4, 3);
        assert_eq!(lines(&v), ["abcd", "efgh", ""]);
        v.add(b"z");
        assert_eq!(lines(&v), ["abcd", "zfgh", ""]);
    }
}
