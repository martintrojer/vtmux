//! Dead panes: what happens when a pane's program goes (`server_destroy_pane`,
//! server-fn.c:353-437), with `remain-on-exit` keeping the pane and drawing
//! `remain-on-exit-format` on its last line, and the key that closes a
//! pane kept by `key` / `failed-key` (`server_client_handle_dead_key`,
//! server-client.c:1411-1428).

use std::io::Write as _;

use vtmux_core::colour::Colour;
use vtmux_core::ids::PaneId;
use vtmux_core::keys::Key;
use vtmux_core::options::{TreeId, Value};
use vtmux_core::style::{Attrs, CellStyle};
use vtmux_core::styled;

use crate::server::{ClientId, Server};
use crate::target::Found;

/// `remain-on-exit`, as the choice list names it (options-table.c:93-95).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Remain {
    Off,
    On,
    Failed,
    Key,
    FailedKey,
}

impl Remain {
    fn from_option(n: i64) -> Self {
        match n {
            1 => Self::On,
            2 => Self::Failed,
            3 => Self::Key,
            4 => Self::FailedKey,
            _ => Self::Off,
        }
    }

    /// Whether a pane whose program ended with `success` stays.
    fn keeps(self, success: bool) -> bool {
        match self {
            Self::Off => false,
            Self::On | Self::Key => true,
            Self::Failed | Self::FailedKey => !success,
        }
    }
}

/// The column a [`styled::draw_onto`] left as it was.
const KEEP: &str = "\u{0}";

impl Server {
    /// `server_destroy_pane(wp, notify)` for a pane whose program went: the
    /// PTY is closed; nothing more happens until the exit status is in
    /// (`PANE_STATUSREADY`). Then `remain-on-exit` decides: a kept pane
    /// fires pane-died (with `notify`) and gets its dead line once
    /// (`PANE_STATUSDRAWN`); otherwise pane-exited (with `notify`) and the
    /// pane goes, taking an emptied window with it.
    pub(crate) fn destroy_exited_pane(&mut self, p: PaneId, notify: bool) {
        self.hangup_pane(p);
        self.pipe_close(p);
        let Some(pane) = self.model.panes.get(&p) else {
            return;
        };
        let Some(status) = pane.status else { return };
        let remain =
            Remain::from_option(self.model.number_option(TreeId::Pane(p), "remain-on-exit"));
        if remain.keeps(status.success()) {
            if pane.dead_time.is_some() {
                return;
            }
            self.model
                .panes
                .get_mut(&p)
                .expect("checked above")
                .dead_time = Some(crate::model::now_epoch());
            if notify {
                self.fire_pane_exit("pane-died", p);
            }
            if self.draw_dead_line(p) {
                self.dead_line_queued(p);
            }
            // server-fn.c:419: PANE_REDRAW.
            self.model.redraw_panes.insert(p);
            return;
        }
        if notify {
            self.fire_pane_exit("pane-exited", p);
        }
        self.destroy_pane(p);
    }

    /// server-fn.c:403-417: the whole screen is the scroll region, the
    /// screen scrolls up a line from the bottom, and the expanded
    /// `remain-on-exit-format` is drawn (`format_draw`, default cell) on the
    /// new last line; the cursor stays at its start, hidden. False when
    /// the format is empty and nothing is drawn.
    pub(crate) fn draw_dead_line(&mut self, p: PaneId) -> bool {
        self.dead_line(p, false)
    }

    /// [`Server::draw_dead_line`] for a pane restored from a checkpoint
    /// (vtmux only, D27), whose screen is the one saved: when its last row
    /// is blank the line goes there without the scroll, so every saved row
    /// stays where it was and none goes into history; otherwise as tmux.
    pub(crate) fn draw_restored_dead_line(&mut self, p: PaneId) -> bool {
        self.dead_line(p, true)
    }

    fn dead_line(&mut self, p: PaneId, restored: bool) -> bool {
        let template = self
            .model
            .options
            .get(TreeId::Pane(p), "remain-on-exit-format")
            .map(|e| e.value.to_text(e.def, None, false))
            .unwrap_or_default();
        let Some(window) = self.model.panes.get(&p).map(|x| x.window) else {
            return false;
        };
        let mut bytes = Vec::new();
        if !template.is_empty() {
            // format_single(NULL, s, NULL, NULL, NULL, wp): the pane and its
            // window, no session or client.
            let found = Found {
                window: Some(window),
                pane: Some(p),
                ..Found::default()
            };
            let ctx = self.format_ctx(found, None);
            let expanded = vtmux_core::format::expand_str(&template, &ctx);
            drop(ctx);
            let pane = &self.model.panes[&p];
            let (sx, sy) = (pane.term.cols(), pane.term.rows());
            let under = vec![
                styled::Cell {
                    text: KEEP.into(),
                    width: 1,
                    style: CellStyle::default(),
                };
                usize::from(sx)
            ];
            let line = styled::draw_onto(&CellStyle::default(), under, &expanded, false, false);
            // screen_write_scrollregion, cursormove to the last line,
            // linefeed (IND: scrolls the region), then the drawn cells. Drawing
            // is not the program's: plain characters, no insert mode,
            // wrapping on, no left and right margins, no link.
            let _ = write!(
                bytes,
                "\x0f\x1b(B\x1b]8;;\x1b\\\x1b[0m\x1b[4l\x1b[?7h\x1b[?69l\x1b[r\x1b[{sy};1H"
            );
            if !(restored && bottom_row_blank(&pane.term, sy)) {
                bytes.extend_from_slice(b"\x1bD");
                if sy >= 2 {
                    mark_wrapped(&mut bytes, &pane.term, sx, sy);
                }
            }
            write_cells(&mut bytes, &line.cells, sy);
            let _ = write!(bytes, "\x1b[0m\x1b[{sy};1H");
        }
        // server-fn.c:417: MODE_CURSOR off.
        bytes.extend_from_slice(b"\x1b[?25l");
        let pane = self.model.panes.get_mut(&p).expect("checked above");
        // screen_write draws cells, not bytes through the pane's parser: a
        // sequence the program left unfinished must not swallow them.
        pane.term.reset_input();
        pane.term.feed(&bytes);
        pane.compress.observe(&pane.term, std::time::Instant::now());
        // Nothing can be answered: the PTY is gone.
        drop(pane.term.take_replies());
        drop(pane.term.take_passthrough());
        pane.term.take_events();
        !template.is_empty()
    }

    /// The dead line is written to the terminal of each client showing pane
    /// `p` as it is drawn (`tty_write`, unless the client is frozen or
    /// already redrawing its window: `tty_client_ready`, tty.c:1630-1649),
    /// so `server_client_check_redraw` finds output still queued there and
    /// defers the pane's `PANE_REDRAW` as `CLIENT_REDRAWWINDOW`
    /// (server-client.c:2537-2556). The whole window is drawn again, its
    /// borders too: a border style reading `#{pane_dead}` changes.
    fn dead_line_queued(&mut self, p: PaneId) {
        let Some(w) = self.model.panes.get(&p).map(|x| x.window) else {
            return;
        };
        if !self.model.windows.get(&w).is_some_and(|x| x.is_visible(p)) {
            return;
        }
        let model = &self.model;
        for client in self.clients.values_mut() {
            let shows = client.control.is_none()
                && !client.status.is_frozen()
                && client
                    .attached
                    .and_then(|s| model.current_of(s))
                    .is_some_and(|(_, cw, _)| cw == w);
            if shows {
                client.status.request_window_redraw();
            }
        }
    }

    /// `server_client_handle_dead_key`: a key (not a mouse event or paste
    /// marker) on client `id` whose active pane is exited and kept by
    /// `key` or `failed-key` turns the option off on the pane and destroys
    /// it, firing nothing. True when the key was taken.
    pub(crate) fn dead_pane_key(&mut self, id: ClientId, key: Key) -> bool {
        if key.is_mouse() || key == Key::PASTE_START || key == Key::PASTE_END {
            return false;
        }
        let Some(p) = self.client_pane(id) else {
            return false;
        };
        let Some(pane) = self.model.panes.get(&p) else {
            return false;
        };
        if !pane.exited && pane.io.is_some() {
            return false;
        }
        let remain =
            Remain::from_option(self.model.number_option(TreeId::Pane(p), "remain-on-exit"));
        if !matches!(remain, Remain::Key | Remain::FailedKey) {
            return false;
        }
        self.model
            .options
            .set_table_value(TreeId::Pane(p), "remain-on-exit", Value::Choice(0));
        self.destroy_exited_pane(p, false);
        true
    }
}

/// `screen_write_linefeed(ctx, 1, ...)` marks the line it leaves wrapped,
/// so the dead line joins it (`capture-pane -J`, reflow). Ghostty marks a
/// row wrapped only when a print wraps: the cell at the end of that line
/// (row `sy - 1` now, 1-based) is printed again as it is, then once more,
/// which moves down (not scrolls) to the new last line, where ECH clears
/// the stray blank. An empty cell past the cells the line used (tmux's
/// `cellused`, which the linefeed leaves as it was) gets a wide character
/// that does not fit, which leaves the cell unwritten (Ghostty's spacer
/// head, the cell tmux leaves when a wide character wraps) and the used
/// cells as they were, so a reflow joins only the line's used cells to
/// the dead line, as `grid_reflow_join` does; ECH clears the character
/// on the new last line. A used empty cell is printed as a blank and
/// cleared by ICH, which leaves the row wrapped (ECH and EL would not) and
/// keeps the cell's background. A cell this cannot write back (wide, a
/// link, the line-drawing set, a styled blank) stays, and its row is not
/// marked (compat/DIVERGENCES.md D-023).
fn mark_wrapped(out: &mut Vec<u8>, term: &vtmux_vt::PaneTerm, sx: u16, sy: u16) {
    // Read before the linefeed is fed: still the bottom row.
    let row = term.row(term.history_size() + usize::from(sy) - 1);
    let Some(cell) = row.cells.last() else { return };
    if cell.width != 1 || cell.link.is_some() || cell.acs || cell.tab {
        return;
    }
    let blank = vtmux_vt::Style {
        bg: cell.style.bg,
        ..vtmux_vt::Style::default()
    };
    let y = sy - 1;
    if cell.text.is_empty() {
        if cell.style != blank {
            return;
        }
        // ECH over the whole new last line would unwrap the marked one
        // (grid_clear_lines), so the wide character needs a third column.
        if sx >= 3 && term.bottom_row_used() < sx {
            let _ = write!(
                out,
                "\x1b[{y};{sx}H{}\u{4e00}\x1b[0m\x1b[{sy};1H\x1b[2X",
                cell_sgr(&blank)
            );
            return;
        }
        let _ = write!(
            out,
            "\x1b[{y};{sx}Hx \x1b[{y};{sx}H{}\x1b[@",
            cell_sgr(&blank)
        );
    } else {
        let _ = write!(
            out,
            "\x1b[{y};{sx}H{}{}\x1b[0m ",
            cell_sgr(&cell.style),
            cell.text
        );
    }
    let _ = write!(out, "\x1b[0m\x1b[{sy};1H\x1b[X");
}

/// Whether the bottom row of `term` (`sy` rows) was never written: no
/// cell used (tmux's `cellused`), none with text, a style or a link, and
/// not wrapped.
fn bottom_row_blank(term: &vtmux_vt::PaneTerm, sy: u16) -> bool {
    let row = term.row(term.history_size() + usize::from(sy) - 1);
    term.bottom_row_used() == 0
        && !row.wrapped
        && row
            .cells
            .iter()
            .all(|c| c.text.is_empty() && c.style == vtmux_vt::Style::default() && c.link.is_none())
}

/// The SGR that gives a grid cell `style`, from a reset.
fn cell_sgr(style: &vtmux_vt::Style) -> String {
    use vtmux_vt::{Color, Underline};
    let colour = |c: Color, base: u16| -> Option<String> {
        Some(match c {
            Color::Default => return None,
            Color::Palette(n @ 0..8) if base != 50 => (base + u16::from(n)).to_string(),
            Color::Palette(n @ 8..16) if base != 50 => (base + 52 + u16::from(n)).to_string(),
            Color::Palette(n) | Color::Palette256(n) => format!("{};5;{n}", base + 8),
            Color::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", base + 8),
        })
    };
    let mut codes = vec!["0".to_owned()];
    for (on, code) in [
        (style.bold, "1"),
        (style.faint, "2"),
        (style.italic, "3"),
        (style.blink, "5"),
        (style.inverse, "7"),
        (style.invisible, "8"),
        (style.strikethrough, "9"),
        (style.overline, "53"),
    ] {
        if on {
            codes.push(code.to_owned());
        }
    }
    codes.extend(
        match style.underline {
            Underline::None => None,
            Underline::Single => Some("4"),
            Underline::Double => Some("4:2"),
            Underline::Curly => Some("4:3"),
            Underline::Dotted => Some("4:4"),
            Underline::Dashed => Some("4:5"),
        }
        .map(str::to_owned),
    );
    codes.extend(colour(style.fg, 30));
    codes.extend(colour(style.bg, 40));
    codes.extend(colour(style.underline_color, 50));
    format!("\x1b[{}m", codes.join(";"))
}

/// `format_draw`'s cells written at row `y` (1-based) as escapes: a move to
/// each run of drawn columns, an SGR for each change of style. ACS cells go
/// through the DEC special graphics set, as `tty_attributes` draws them.
fn write_cells(out: &mut Vec<u8>, cells: &[styled::Cell], y: u16) {
    let mut at: Option<usize> = None;
    let mut style: Option<&CellStyle> = None;
    let mut x = 0;
    for cell in cells {
        let width = usize::from(cell.width);
        if cell.text == KEEP || cell.is_padding() {
            x += width;
            continue;
        }
        if at != Some(x) {
            let _ = write!(out, "\x1b[{y};{}H", x + 1);
        }
        if style != Some(&cell.style) {
            out.extend_from_slice(sgr(&cell.style).as_bytes());
            style = Some(&cell.style);
        }
        out.extend_from_slice(cell.text.as_bytes());
        x += width.max(1);
        at = Some(x);
    }
    if style.is_some_and(|s| s.attrs.contains(Attrs::ACS)) {
        out.extend_from_slice(b"\x1b(B");
    }
}

/// The SGR (and character set) that gives a cell `style`, from a reset.
fn sgr(style: &CellStyle) -> String {
    let a = style.attrs;
    let mut codes = vec!["0".to_owned()];
    for (attr, code) in [
        (Attrs::BRIGHT, "1"),
        (Attrs::DIM, "2"),
        (Attrs::ITALICS, "3"),
        (Attrs::UNDERSCORE, "4"),
        (Attrs::DOUBLE_UNDERSCORE, "4:2"),
        (Attrs::CURLY_UNDERSCORE, "4:3"),
        (Attrs::DOTTED_UNDERSCORE, "4:4"),
        (Attrs::DASHED_UNDERSCORE, "4:5"),
        (Attrs::BLINK, "5"),
        (Attrs::REVERSE, "7"),
        (Attrs::HIDDEN, "8"),
        (Attrs::STRIKETHROUGH, "9"),
        (Attrs::OVERLINE, "53"),
    ] {
        if a.contains(attr) {
            codes.push(code.to_owned());
        }
    }
    codes.extend(colour(style.fg, 30));
    codes.extend(colour(style.bg, 40));
    codes.extend(colour(style.us, 50));
    let charset = if a.contains(Attrs::ACS) {
        "\x1b(0"
    } else {
        "\x1b(B"
    };
    format!("\x1b[{}m{charset}", codes.join(";"))
}

/// A colour's SGR parameters at `base` (30 foreground, 40 background, 50
/// underline, which has only the extended forms); none for the default.
fn colour(c: Colour, base: u16) -> Option<String> {
    Some(match c {
        Colour::Default | Colour::Terminal | Colour::Theme(_) => return None,
        Colour::Ansi(n) if base != 50 => (base + u16::from(n)).to_string(),
        Colour::Bright(n) if base != 50 => (base + 60 + u16::from(n)).to_string(),
        Colour::Ansi(n) | Colour::Palette(n) => format!("{};5;{n}", base + 8),
        Colour::Bright(n) => format!("{};5;{}", base + 8, n + 8),
        Colour::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", base + 8),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_exits_keep_the_pane() {
        let keeps = |n, ok| Remain::from_option(n).keeps(ok);
        assert!(!keeps(0, false));
        assert!(keeps(1, true) && keeps(1, false));
        assert!(!keeps(2, true) && keeps(2, false));
        assert!(keeps(3, true) && keeps(3, false));
        assert!(!keeps(4, true) && keeps(4, false));
    }

    /// The bytes drawn back through a terminal give the cells drawn.
    #[test]
    fn drawn_cells_round_trip_through_a_terminal() {
        let line = styled::draw(
            &CellStyle::default(),
            12,
            "#[fg=red,bold]ab#[default] #[bg=colour100,reverse]c#[align=right]#[acs]q",
            false,
            false,
        );
        let mut bytes = b"\x1b[2;1H".to_vec();
        write_cells(&mut bytes, &line.cells, 1);
        let mut t = vtmux_vt::PaneTerm::new(12, 2, 0);
        t.feed(&bytes);
        let row = t.row(0);
        // Untouched columns stay empty; `q` in the line-drawing set is the
        // glyph Ghostty stores.
        assert_eq!(row.text(), "ab c       \u{2500}");
        assert!(row.cells[0].style.bold);
        assert_eq!(row.cells[0].style.fg, vtmux_vt::Color::Palette(1));
        assert!(!row.cells[2].style.bold);
        assert_eq!(row.cells[3].style.bg, vtmux_vt::Color::Palette(100));
        assert!(row.cells[3].style.inverse);
        assert!(row.cells[11].acs);
    }
}
