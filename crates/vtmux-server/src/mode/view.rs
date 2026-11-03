//! View mode (`window_view_mode`, window-copy.c:192-203): copy mode over a
//! grid of its own that command output is appended to, rather than a copy
//! of the pane. `cmdq_print` from an attached client and `run-shell` output
//! to a pane land here (`server_client_print`, `cmd_run_shell_print`).
//!
//! tmux writes each line into the mode's backing screen through an input
//! parser (`window_copy_vadd`, window-copy.c:715-760), so escapes in the
//! text are interpreted. Here the mode keeps a private terminal the text
//! is fed to, and the rows it wrote are read into the copy's grid after
//! each batch of lines (all of one command's prints, a `run-shell`'s
//! whole output).
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
    #[cfg(test)]
    pub fn add(&mut self, text: &[u8]) {
        self.add_lines([text]);
    }

    /// [`Self::add`] for each of `lines`, read into the copy once at the
    /// end: the lines above the first one's history are as they were, so
    /// only the rows from there on are read, and a long output costs the
    /// rows it wrote rather than a screen per line.
    pub fn add_lines<'a>(&mut self, lines: impl IntoIterator<Item = &'a [u8]>) {
        let from = (!self.stale).then(|| self.term.history_size());
        let mut bytes = Vec::new();
        let mut any = false;
        for text in lines {
            if self.written {
                bytes.extend_from_slice(b"\r\n");
            }
            (self.written, any) = (true, true);
            bytes.extend_from_slice(text);
        }
        if !any {
            return;
        }
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
        m.add_lines(lines);
        self.model.redraw_panes.insert(p);
    }

    /// The prints among `output` (one command's, for an attached terminal
    /// client of session `s`) taken out and added to view mode in one go
    /// ([`Self::view_print`]).
    pub(crate) fn view_print_outputs(
        &mut self,
        s: vtmux_core::ids::SessionId,
        output: &mut Vec<crate::commands::Output>,
    ) {
        let mut texts = Vec::new();
        output.retain_mut(|o| match o {
            crate::commands::Output::Print(text) => {
                texts.push(std::mem::take(text));
                false
            }
            _ => true,
        });
        if !texts.is_empty() {
            self.view_print(s, &texts);
        }
    }

    /// `server_client_print` with `parse` set for an attached terminal
    /// client (server-client.c:3260-3278), for each of `texts`: the text
    /// goes to the view mode of its session's active pane, one line per
    /// LF-ended piece, then the rest. tmux adds a NUL to non-empty text
    /// first, so the rest is never empty: a text ending in LF adds an empty
    /// line after it, an empty text adds nothing (view mode is still
    /// entered). Each line stops at a NUL (`%s`). The lines of all the
    /// texts are added together.
    pub(crate) fn view_print(&mut self, s: vtmux_core::ids::SessionId, texts: &[Vec<u8>]) {
        let Some(p) = crate::target::Found::from_session(&self.model, s).pane else {
            return;
        };
        let lines = texts
            .iter()
            .filter(|text| !text.is_empty())
            .flat_map(|text| text.split(|&b| b == b'\n'))
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

    /// Rows read into the copy while `f` runs.
    fn rows_read(f: impl FnOnce()) -> usize {
        let read = || super::super::grid::tests::ROWS_READ.with(std::cell::Cell::get);
        let before = read();
        f();
        read() - before
    }

    /// A batch of lines costs the rows it wrote, not a screen per line:
    /// 20 000 lines read into an 80x24 copy once (`run-shell 'seq 1
    /// 50000'` blocked the server for 15 s re-reading the screen after
    /// every line). One more line after them reads the screen it scrolled
    /// and nothing of the history above it. The view stays at the top.
    #[test]
    fn a_batch_reads_each_new_row_once() {
        let mut v = ViewMode::new((80, 24), false, None);
        let text: Vec<Vec<u8>> = (1..=20_000).map(|i| i.to_string().into_bytes()).collect();
        let read = rows_read(|| v.add_lines(text.iter().map(Vec::as_slice)));
        assert_eq!(read, 20_000);
        assert_eq!(rows_read(|| v.add(b"z")), 24 + 1);
        assert_eq!(lines(&v)[..2], ["1", "2"]);
        assert_eq!(v.copy.position(0), (20_000 - 24 + 1, 20_000 - 24 + 1));
    }

    /// A command's prints go to view mode together: 2 000 one-line
    /// prints read 2 000 rows, and the output that is not a print stays.
    #[test]
    fn one_commands_prints_are_one_batch() {
        use crate::commands::Output;
        let (mut server, p, s, _, dir) = crate::mode::prompt::tests::setup();
        let mut output: Vec<Output> = (1..=2_000)
            .map(|i| Output::Print(i.to_string().into_bytes()))
            .collect();
        output.push(Output::Stderr(b"e".to_vec()));
        let read = rows_read(|| server.view_print_outputs(s, &mut output));
        assert!(matches!(output[..], [Output::Stderr(_)]));
        let Some(Mode::View(v)) = server.model.panes[&p].modes.first() else {
            panic!("not in view mode");
        };
        assert_eq!(lines(v)[..2], ["1", "2"]);
        assert_eq!(read, 2_000);
        std::fs::remove_dir_all(dir).ok();
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
