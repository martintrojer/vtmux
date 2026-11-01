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

use super::scene::{Lines, Scene, SceneCell, Shape};

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
            acs: c.style.attrs.contains(Attrs::ACS),
            link: c.style.link.is_some(),
            tab: false,
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
    /// Per content cell, row-major: in copy mode's selection
    /// (`GRID_FLAG_SELECTED`), so never erased as part of an empty run.
    /// Empty without a selection.
    pub selected: Vec<bool>,
}

/// Everything [`compose`] needs besides the scene.
#[derive(Clone, Debug)]
pub struct Draw {
    pub panes: BTreeMap<PaneId, PaneDraw>,
    pub active: Option<PaneId>,
    /// Where the window's origin is in the drawn area: 1 cell in under the
    /// focus ring (D23), else 0.
    pub offset: u16,
    /// The window's `pane-border-style` and `pane-border-lines`
    /// (`redraw_get_default_border_style`).
    pub border: CellStyle,
    pub lines: Lines,
    /// `fill-character` outside the window and inside it
    /// (window-border.c `window_set_fill_cells`).
    pub outside: styled::Cell,
    pub inside: styled::Cell,
    /// How empty runs are cleared ([`Clear`]).
    pub clear: Clear,
}

/// How `tty_draw_line_clear` (tty-draw.c:52-90) clears a run of ten or
/// more empty cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clear {
    /// Genuine background colour erase: any run is erased.
    Bce,
    /// No BCE (`tty_fake_bce`): only a run in the default background is.
    FakeBce,
    /// An overlay is up: never erased, written as spaces.
    Spaces,
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
    // The last row drawn left the terminal cursor past its last column.
    let mut ended_written = false;
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
                        .map_or_else(|| b.shape().glyph(lines, index), Into::into);
                    Cell {
                        text,
                        width: 1,
                        style: theme.style(style),
                        ..Cell::default()
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
        let above_ended_written = std::mem::take(&mut ended_written);
        while x < width {
            let Some(key) = span_key(scene.get(x, y), x) else {
                x += 1;
                continue;
            };
            let end = (x..width)
                .find(|&e| span_key(scene.get(e, y), e) != Some(key))
                .unwrap_or(width);
            let row = usize::from(y) * cols;
            let full = x == 0 && end == screen.cols;
            // tty_draw_line's `wrapped`: a full-width row of a line the row
            // above wrapped onto, with the terminal cursor left past the
            // end of that row.
            let wrapped = full
                && above_ended_written
                && match scene.get(x, y) {
                    SceneCell::Pane { pane, py, .. } => py
                        .checked_sub(1)
                        .and_then(|above| {
                            d.panes
                                .get(pane)?
                                .content
                                .wrapped
                                .get(usize::try_from(above).ok()?)
                                .copied()
                        })
                        .unwrap_or(false),
                    _ => false,
                };
            let cells = &mut screen.cells[row + usize::from(x)..row + usize::from(end)];
            let bgs: Vec<Color> = cells.iter().map(|c| c.style.bg).collect();
            let keep = selected_span(d, scene, x, end, y);
            let written = draw_line_keeping(cells, &bgs, Color::Default, d.clear, wrapped, &keep);
            ended_written = full && written;
            x = end;
        }
    }
}

/// Which cells of the span `x..end` of row `y` are [`PaneDraw::selected`].
fn selected_span(d: &Draw, scene: &Scene, x: u16, end: u16, y: u16) -> Vec<bool> {
    (x..end)
        .map(|x| match scene.get(x, y) {
            SceneCell::Pane { pane, px, py } => d.panes.get(pane).is_some_and(|p| {
                let cols = usize::from(p.content.cols);
                let at =
                    usize::try_from(*py).unwrap_or(0) * cols + usize::try_from(*px).unwrap_or(0);
                p.selected.get(at).copied().unwrap_or(false)
            }),
            _ => false,
        })
        .collect()
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
/// no attributes (the charset counts, `GRID_ATTR_CHARSET`), no hyperlink,
/// and the background of the cell before it (`bgs`, as the grid stores
/// it); the foreground does not count. Snapshots carry no tab flag.
fn is_empty<T: PartialEq>(c: &Cell, bg: &T, last: &T) -> bool {
    let plain = Style {
        fg: c.style.fg,
        bg: c.style.bg,
        underline_color: c.style.underline_color,
        ..Style::default()
    };
    c.width == 1
        && !c.acs
        && !c.link
        && (c.text.is_empty() || c.text == " ")
        && bg == last
        && c.style == plain
}

/// The cells of one span as the terminal shows them after tty-draw.c
/// `tty_draw_line`: a run of empty cells is cleared with the last one's
/// attributes (`tty_draw_line_clear`). Ten or more are erased (EL/EL1/ECH)
/// unless the terminal would need a fake BCE, and keep only the background,
/// no text; shorter runs are spaces in the last cell's style. Erased cells
/// are the only empty-text cells left, so the compositor erases them too.
///
/// `wrapped` (the row continues a line wrapped onto it, see [`compose`])
/// writes a leading run as spaces whatever its length; it lasts until the
/// first flush, so only a run at the start of the row.
///
/// Returns whether the last cell was written, not erased: the terminal
/// cursor is then past the end of the span.
#[cfg(test)]
pub(crate) fn draw_line(cells: &mut [Cell], bce: bool, wrapped: bool) -> bool {
    let bgs: Vec<Color> = cells.iter().map(|c| c.style.bg).collect();
    let clear = if bce { Clear::Bce } else { Clear::FakeBce };
    draw_line_by(cells, &bgs, Color::Default, clear, wrapped)
}

/// [`draw_line`] where "the same background" is decided on `bgs`, one per
/// cell, starting from `default` (`defaults->bg`). tmux compares grid
/// colours, before a theme colour becomes what the terminal is sent, so
/// `bg=green` next to a theme green that resolves to the same colour still
/// differs (status lines, where both meet).
pub(crate) fn draw_line_by<T: PartialEq + Clone>(
    cells: &mut [Cell],
    bgs: &[T],
    default: T,
    clear: Clear,
    wrapped: bool,
) -> bool {
    draw_line_keeping(cells, bgs, default, clear, wrapped, &[])
}

/// [`draw_line_by`] where the cells `keep` marks are never empty: a
/// selected cell is always written (`tty_draw_line_get_empty`,
/// tty-draw.c:105-106).
fn draw_line_keeping<T: PartialEq + Clone>(
    cells: &mut [Cell],
    bgs: &[T],
    default: T,
    clear: Clear,
    mut wrapped: bool,
    keep: &[bool],
) -> bool {
    let empty = |cells: &[Cell], i: usize, last: &T| {
        !keep.get(i).copied().unwrap_or(false) && is_empty(&cells[i], &bgs[i], last)
    };
    let mut last_bg = default;
    let mut written = false;
    let mut i = 0;
    while i < cells.len() {
        if cells[i].width == 0 {
            i += 1;
            continue;
        }
        if !empty(cells, i, &last_bg) {
            if cells[i].text.is_empty() {
                cells[i].text = " ".into();
            }
            last_bg = bgs[i].clone();
            i += usize::from(cells[i].width);
            wrapped = false;
            written = true;
            continue;
        }
        let start = i;
        while i < cells.len() && empty(cells, i, &last_bg) {
            i += 1;
        }
        let last = cells[i - 1].style;
        let erase = !wrapped
            && i - start >= 10
            && match clear {
                Clear::Bce => true,
                Clear::FakeBce => last.bg == Color::Default,
                Clear::Spaces => false,
            };
        wrapped = false;
        written = !erase;
        let cell = if erase {
            Cell {
                text: String::new(),
                width: 1,
                style: Style {
                    bg: last.bg,
                    ..Style::default()
                },
                ..Cell::default()
            }
        } else {
            Cell {
                text: " ".into(),
                width: 1,
                style: last,
                ..Cell::default()
            }
        };
        cells[start..i].fill(cell);
    }
    written
}

/// `window_make_pane_status`: the border line under the status line, then
/// `expanded` (`pane-border-format`) drawn over it in the border style.
/// `under` is [`Scene::status_line`]. Under the focus ring (D23) the line
/// cells on the active pane's rectangle take `ring` (the active border
/// style), so the rectangle stays one colour behind another pane's label.
#[must_use]
pub fn pane_status(
    under: &[Option<Shape>],
    border: &CellStyle,
    ring: &CellStyle,
    lines: Lines,
    index: Option<u32>,
    expanded: &str,
) -> Vec<styled::Cell> {
    if under.is_empty() {
        return Vec::new();
    }
    let row = under
        .iter()
        .map(|j| {
            let shape = j.unwrap_or_else(|| super::scene::Junction::Horizontal.into());
            styled::Cell {
                text: shape.glyph(lines, index),
                width: 1,
                style: if shape.heavy() { ring } else { border }.clone(),
            }
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
    use crate::render::scene::Junction;

    /// A status line shorter than its format is cut; past its text the
    /// border shows through, in the border style.
    #[test]
    fn status_line_draws_over_the_border() {
        let green = CellStyle {
            fg: Colour::Ansi(2),
            ..CellStyle::default()
        };
        let under = [Some(Junction::Horizontal.into()); 6];
        let cells = pane_status(
            &under,
            &green,
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
            ..Cell::default()
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
            draw_line(&mut row, true, false);
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
        draw_line(&mut row, true, false);
        assert_eq!(row[0].text, " ", "first blue space follows a default bg");
        assert!(row[1..].iter().all(|c| c.text.is_empty()));
        let mut row: Vec<Cell> = (0..12)
            .map(|_| text_cell(" ", Color::Default, blue))
            .collect();
        draw_line(&mut row, false, false);
        assert!(row.iter().all(|c| c.text == " "));
        let mut row: Vec<Cell> = (0..12)
            .map(|_| text_cell(" ", Color::Default, Color::Default))
            .collect();
        row[5].style.bold = true;
        draw_line(&mut row, true, false);
        assert!(row.iter().all(|c| c.text == " "));
    }

    /// Spaces under DEC graphics or inside a hyperlink are not empty, so a
    /// run of them is written with its foreground (`screen_pane_link_acs`).
    #[test]
    fn charset_and_link_spaces_are_not_empty() {
        let red = Color::Palette(1);
        for mark in [|c: &mut Cell| c.acs = true, |c: &mut Cell| c.link = true] {
            let mut row: Vec<Cell> = (0..12)
                .map(|_| text_cell(" ", red, Color::Default))
                .collect();
            row.iter_mut().for_each(mark);
            draw_line(&mut row, true, false);
            assert!(row.iter().all(|c| c.text == " " && c.style.fg == red));
        }
    }

    /// A wrapped row writes its leading run as spaces in the last cell's
    /// style; a later run is erased as usual (`screen_pane_wrap`).
    #[test]
    fn wrapped_row_writes_only_its_leading_run() {
        let red = Color::Palette(1);
        let mut row: Vec<Cell> = (0..12)
            .map(|_| text_cell(" ", red, Color::Default))
            .collect();
        row.push(text_cell("y", red, Color::Default));
        row.extend((0..12).map(|_| text_cell(" ", red, Color::Default)));
        assert!(!draw_line(&mut row, true, true));
        assert!(row[..12].iter().all(|c| c.text == " " && c.style.fg == red));
        assert!(row[13..].iter().all(|c| c.text.is_empty()));
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
