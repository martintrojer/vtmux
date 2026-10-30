//! Turn a [`Scene`] into cells (screen-redraw.c `redraw_draw`): pane content,
//! border glyphs in their pane's style, arrows, pane status lines and the
//! window fill.
//!
//! Styles stay [`CellStyle`]s (theme colours unresolved) until here, where a
//! client's [`Theme`] turns them into terminal colours.

use std::collections::BTreeMap;

use vtmux_core::colour::Colour;
use vtmux_core::ids::PaneId;
use vtmux_core::style::{Attrs, CellStyle};
use vtmux_core::styled;
use vtmux_vt::{Cell, Color, Snapshot, Style, Underline};

use super::scene::{Junction, Lines, Scene, SceneCell, glyph};

/// A client's ten `theme*` colours (`c->theme_colours`,
/// server-client.c `server_client_update_theme_colours`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Theme(pub [Color; 10]);

impl Theme {
    /// Colour as the terminal sees it (colour.c `colour_toescape`).
    #[must_use]
    pub fn colour(&self, c: Colour) -> Color {
        match c {
            Colour::Default | Colour::Terminal => Color::Default,
            Colour::Ansi(n) | Colour::Palette(n) => Color::Palette(n),
            Colour::Bright(n) => Color::Palette(8 + n),
            Colour::Rgb(r, g, b) => Color::Rgb(r, g, b),
            Colour::Theme(i) => self.0.get(usize::from(i)).copied().unwrap_or_default(),
        }
    }

    /// A cell style as terminal attributes.
    #[must_use]
    pub fn style(&self, s: &CellStyle) -> Style {
        let a = s.attrs;
        let underline = if a.contains(Attrs::DOUBLE_UNDERSCORE) {
            Underline::Double
        } else if a.contains(Attrs::CURLY_UNDERSCORE) {
            Underline::Curly
        } else if a.contains(Attrs::DOTTED_UNDERSCORE) {
            Underline::Dotted
        } else if a.contains(Attrs::DASHED_UNDERSCORE) {
            Underline::Dashed
        } else if a.contains(Attrs::UNDERSCORE) {
            Underline::Single
        } else {
            Underline::None
        };
        Style {
            fg: self.colour(s.fg),
            bg: self.colour(s.bg),
            underline_color: self.colour(s.us),
            bold: a.contains(Attrs::BRIGHT),
            faint: a.contains(Attrs::DIM),
            italic: a.contains(Attrs::ITALICS),
            blink: a.contains(Attrs::BLINK),
            inverse: a.contains(Attrs::REVERSE),
            invisible: a.contains(Attrs::HIDDEN),
            strikethrough: a.contains(Attrs::STRIKETHROUGH),
            overline: a.contains(Attrs::OVERLINE),
            underline,
        }
    }

    /// A drawn format cell; `acs` text goes through the UTF-8 line-drawing
    /// table (tty-acs.c `tty_acs_get`).
    #[must_use]
    pub fn cell(&self, c: &styled::Cell) -> Cell {
        let text = if c.style.attrs.contains(Attrs::ACS) {
            c.text.chars().map(acs).collect()
        } else {
            c.text.clone()
        };
        Cell {
            text,
            width: c.width,
            style: self.style(&c.style),
        }
    }
}

/// tty-acs.c `tty_acs_table`.
fn acs(c: char) -> char {
    match c {
        '+' => '→',
        ',' => '←',
        '-' => '↑',
        '.' => '↓',
        '0' => '▮',
        '`' => '◆',
        'a' => '▒',
        'f' => '°',
        'g' => '±',
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        c => c,
    }
}

/// How one visible pane draws.
#[derive(Clone, Debug)]
pub struct PaneDraw {
    pub content: Snapshot,
    /// `pane_index`, for `number` lines.
    pub index: Option<u32>,
    pub lines: Lines,
    /// `pane-active-border-style` for the active pane, else
    /// `pane-border-style` (window-border.c `window_pane_get_border_style`).
    pub border: CellStyle,
    /// The drawn status line, empty when there is none.
    pub status: Vec<styled::Cell>,
}

/// Everything [`compose`] needs besides the scene.
#[derive(Clone, Debug)]
pub struct Draw {
    pub panes: BTreeMap<PaneId, PaneDraw>,
    pub active: Option<PaneId>,
    /// The window's `pane-border-style` and `pane-border-lines`
    /// (`redraw_get_default_border_style`).
    pub border: CellStyle,
    pub lines: Lines,
    /// `fill-character` outside the window and inside it
    /// (window-border.c `window_set_fill_cells`).
    pub outside: styled::Cell,
    pub inside: styled::Cell,
    /// The terminal has genuine background colour erase (tty.c
    /// `tty_fake_bce`).
    pub bce: bool,
}

/// `window_copy_fill_cell`: the fill's text and attributes over `base`, its
/// colours where set.
fn fill(base: &CellStyle, f: &styled::Cell) -> styled::Cell {
    let or = |a: Colour, b: Colour| if a == Colour::Default { b } else { a };
    styled::Cell {
        text: f.text.clone(),
        width: 1,
        style: CellStyle {
            fg: or(f.style.fg, base.fg),
            bg: or(f.style.bg, base.bg),
            us: or(f.style.us, base.us),
            attrs: base.attrs.union(f.style.attrs),
            link: base.link.clone(),
        },
    }
}

/// Draw `scene` into the top `scene.rows` rows of `screen`.
pub fn compose(scene: &Scene, d: &Draw, theme: &Theme, screen: &mut Snapshot) {
    let cols = usize::from(screen.cols);
    for y in 0..scene.rows.min(screen.rows) {
        for x in 0..scene.cols.min(screen.cols) {
            let cell = match scene.get(x, y) {
                SceneCell::Pane { pane, px, py } => d
                    .panes
                    .get(pane)
                    .and_then(|p| {
                        p.content
                            .cell(u16::try_from(*px).ok()?, u16::try_from(*py).ok()?)
                    })
                    .cloned()
                    .unwrap_or_default(),
                SceneCell::Outside => theme.cell(&fill(&d.border, &d.outside)),
                SceneCell::Empty => theme.cell(&fill(&d.border, &d.inside)),
                SceneCell::Border(b) => {
                    let owner = b.style_pane(d.active).and_then(|p| d.panes.get(&p));
                    let (style, lines, index) = owner.map_or((&d.border, d.lines, None), |p| {
                        (&p.border, p.lines, p.index)
                    });
                    let text = b
                        .arrow_glyph(d.active)
                        .map_or_else(|| glyph(lines, b.junction(), index), Into::into);
                    Cell {
                        text,
                        width: 1,
                        style: theme.style(style),
                    }
                }
                SceneCell::Status { pane, offset, .. } => d
                    .panes
                    .get(pane)
                    .and_then(|p| p.status.get(usize::try_from(*offset).ok()?))
                    .map_or_else(Cell::default, |c| theme.cell(c)),
            };
            screen.cells[usize::from(y) * cols + usize::from(x)] = cell;
        }
        // Pane and pane status spans go through `tty_draw_line`; borders and
        // fill are drawn cell by cell (`tty_cell`) and keep their text.
        let width = scene.cols.min(screen.cols);
        let mut x = 0;
        while x < width {
            let Some(key) = span_key(scene.get(x, y), x) else {
                x += 1;
                continue;
            };
            let end = (x..width)
                .find(|&e| span_key(scene.get(e, y), e) != Some(key))
                .unwrap_or(width);
            let row = usize::from(y) * cols;
            draw_line(
                &mut screen.cells[row + usize::from(x)..row + usize::from(end)],
                d.bce,
            );
            x = end;
        }
    }
}

/// What makes cells one `tty_draw_line` span: the same pane's content or
/// status line, contiguous in the pane (screen-redraw.c `redraw_build_spans`).
fn span_key(cell: &SceneCell, x: u16) -> Option<(bool, PaneId, i64)> {
    match cell {
        SceneCell::Pane { pane, px, .. } => Some((false, *pane, i64::from(x) - i64::from(*px))),
        SceneCell::Status { pane, offset, .. } => {
            Some((true, *pane, i64::from(x) - i64::from(*offset)))
        }
        _ => None,
    }
}

/// tty-draw.c `tty_draw_line_get_empty`: blank (cleared or a single space),
/// no attributes, and the background of the cell before it; the foreground
/// does not count. Snapshots carry no tab flag or hyperlink.
fn is_empty(c: &Cell, bg: Color) -> bool {
    let plain = Style {
        fg: c.style.fg,
        bg: c.style.bg,
        underline_color: c.style.underline_color,
        ..Style::default()
    };
    c.width == 1 && (c.text.is_empty() || c.text == " ") && c.style.bg == bg && c.style == plain
}

/// The cells of one span as the terminal shows them after tty-draw.c
/// `tty_draw_line`: a run of empty cells is cleared with the last one's
/// attributes (`tty_draw_line_clear`). Ten or more are erased (EL/EL1/ECH)
/// unless the terminal would need a fake BCE, and keep only the background,
/// no text; shorter runs are spaces in the last cell's style. Erased cells
/// are the only empty-text cells left, so the compositor erases them too.
fn draw_line(cells: &mut [Cell], bce: bool) {
    // `last` starts as the default cell.
    let mut last_bg = Color::Default;
    let mut i = 0;
    while i < cells.len() {
        if cells[i].width == 0 {
            i += 1;
            continue;
        }
        if !is_empty(&cells[i], last_bg) {
            if cells[i].text.is_empty() {
                cells[i].text = " ".into();
            }
            last_bg = cells[i].style.bg;
            i += usize::from(cells[i].width);
            continue;
        }
        let start = i;
        while i < cells.len() && is_empty(&cells[i], last_bg) {
            i += 1;
        }
        let last = cells[i - 1].style;
        let erase = i - start >= 10 && (bce || last.bg == Color::Default);
        let cell = if erase {
            Cell {
                text: String::new(),
                width: 1,
                style: Style {
                    bg: last.bg,
                    ..Style::default()
                },
            }
        } else {
            Cell {
                text: " ".into(),
                width: 1,
                style: last,
            }
        };
        cells[start..i].fill(cell);
    }
}

/// `window_make_pane_status`: the border line under the status line, then
/// `expanded` (`pane-border-format`) drawn over it in the border style.
/// `under` is [`Scene::status_line`].
#[must_use]
pub fn pane_status(
    under: &[Option<Junction>],
    border: &CellStyle,
    lines: Lines,
    index: Option<u32>,
    expanded: &str,
) -> Vec<styled::Cell> {
    if under.is_empty() {
        return Vec::new();
    }
    let row = under
        .iter()
        .map(|j| styled::Cell {
            text: glyph(lines, j.unwrap_or(Junction::Horizontal), index),
            width: 1,
            style: border.clone(),
        })
        .collect();
    let base = CellStyle {
        attrs: border.attrs.without(Attrs::ACS),
        ..border.clone()
    };
    styled::draw_onto(&base, row, expanded, false, false).cells
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A status line shorter than its format is cut; past its text the
    /// border shows through, in the border style.
    #[test]
    fn status_line_draws_over_the_border() {
        let green = CellStyle {
            fg: Colour::Ansi(2),
            ..CellStyle::default()
        };
        let under = [Some(Junction::Horizontal); 6];
        let cells = pane_status(
            &under,
            &green,
            Lines::Single,
            None,
            "#[reverse]0#[default] x",
        );
        let text: String = cells.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(text, "0 x───");
        assert!(cells[0].style.attrs.contains(Attrs::REVERSE));
        assert_eq!(cells[1].style.fg, Colour::Ansi(2));
        assert_eq!(cells[5].style, green);
    }

    fn text_cell(text: &str, fg: Color, bg: Color) -> Cell {
        Cell {
            text: text.into(),
            width: 1,
            style: Style {
                fg,
                bg,
                ..Style::default()
            },
        }
    }

    /// `screen_pane_spaces_bg`: `\e[31;44mx` then eleven spaces then `y`: the spaces are
    /// erased with the blue background and lose the red foreground; nine
    /// are written as spaces.
    #[test]
    fn long_space_runs_are_erased_whatever_their_foreground() {
        let (red, blue) = (Color::Palette(1), Color::Palette(4));
        for (n, erased) in [(11, true), (9, false)] {
            let mut row = vec![text_cell("x", red, blue)];
            row.extend((0..n).map(|_| text_cell(" ", red, blue)));
            row.push(text_cell("y", red, blue));
            draw_line(&mut row, true);
            let gap = &row[1..=n];
            if erased {
                assert!(gap.iter().all(|c| c.text.is_empty()
                    && c.style
                        == Style {
                            bg: blue,
                            ..Style::default()
                        }));
            } else {
                assert!(gap.iter().all(|c| c.text == " " && c.style.fg == red));
            }
            assert_eq!((row[0].text.as_str(), row[n + 1].text.as_str()), ("x", "y"));
        }
    }

    /// A space with another background, or with attributes, is not empty;
    /// without BCE a coloured run is written, not erased.
    #[test]
    fn background_change_and_attributes_break_a_run() {
        let blue = Color::Palette(4);
        let mut row: Vec<Cell> = (0..12)
            .map(|_| text_cell(" ", Color::Default, blue))
            .collect();
        draw_line(&mut row, true);
        assert_eq!(row[0].text, " ", "first blue space follows a default bg");
        assert!(row[1..].iter().all(|c| c.text.is_empty()));
        let mut row: Vec<Cell> = (0..12)
            .map(|_| text_cell(" ", Color::Default, blue))
            .collect();
        draw_line(&mut row, false);
        assert!(row.iter().all(|c| c.text == " "));
        let mut row: Vec<Cell> = (0..12)
            .map(|_| text_cell(" ", Color::Default, Color::Default))
            .collect();
        row[5].style.bold = true;
        draw_line(&mut row, true);
        assert!(row.iter().all(|c| c.text == " "));
    }

    #[test]
    fn acs_fill_becomes_a_bullet() {
        let theme = Theme::default();
        let c = styled::Cell {
            text: "~".into(),
            width: 1,
            style: CellStyle {
                attrs: Attrs::ACS,
                ..CellStyle::default()
            },
        };
        assert_eq!(theme.cell(&c).text, "·");
    }
}
