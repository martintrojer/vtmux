//! A popup drawn over the client's screen (popup.c `popup_draw_cb`): its
//! box in `popup-border-lines` and the border style, the title over the
//! top line, and the popup terminal's content inside, its default colours
//! taken from the popup style.
//!
//! Composed into the same frame as the panes (D14), so cells under the
//! popup are never written while it is up.

use vtmux_core::colour::Colour;
use vtmux_core::style::CellStyle;
use vtmux_core::styled;
use vtmux_vt::{Cell, Color, Snapshot};

use super::compose::{Clear, Theme, draw_line_by};

/// `popup-border-lines` (options-table.c:81-83, `enum box_lines`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoxLines {
    Single,
    Double,
    Heavy,
    Simple,
    Rounded,
    Padded,
    None,
}

impl BoxLines {
    /// The choices in the option's order.
    pub const NAMES: [&'static str; 7] = [
        "single", "double", "heavy", "simple", "rounded", "padded", "none",
    ];

    #[must_use]
    pub fn from_choice(n: i64) -> Self {
        match n {
            1 => Self::Double,
            2 => Self::Heavy,
            3 => Self::Simple,
            4 => Self::Rounded,
            5 => Self::Padded,
            6 => Self::None,
            _ => Self::Single,
        }
    }

    /// `options_find_choice` on the option's choices.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        let i = Self::NAMES.iter().position(|n| *n == name)?;
        Some(Self::from_choice(i64::try_from(i).ok()?))
    }

    /// The box's corners and lines (`screen_write_box_border_set`, the
    /// tty-acs.c UTF-8 tables and `SIMPLE_BORDERS`/`PADDED_BORDERS`):
    /// top-left, top-right, bottom-left, bottom-right, horizontal, vertical.
    fn glyphs(self) -> [&'static str; 6] {
        match self {
            Self::Single => ["┌", "┐", "└", "┘", "─", "│"],
            Self::Double => ["╔", "╗", "╚", "╝", "═", "║"],
            Self::Heavy => ["┏", "┓", "┗", "┛", "━", "┃"],
            Self::Simple => ["+", "+", "+", "+", "-", "|"],
            Self::Rounded => ["╭", "╮", "╰", "╯", "─", "│"],
            Self::Padded | Self::None => [" "; 6],
        }
    }
}

/// One popup as it draws.
#[derive(Debug)]
pub struct PopupDraw<'a> {
    /// Top-left corner and size on the client's screen.
    pub px: u16,
    pub py: u16,
    pub sx: u16,
    pub sy: u16,
    pub lines: BoxLines,
    /// `popup-border-style` (and `-S`), colours only (`border_cell`).
    pub border: &'a CellStyle,
    /// `popup-style` (and `-s`), colours only (`pd->defaults`): what the
    /// content's default colours become.
    pub defaults: &'a CellStyle,
    /// The title, already expanded at the command (`-T`).
    pub title: &'a str,
    pub content: &'a Snapshot,
    /// The popup was drawn again in full since it opened
    /// (`popup_draw_cb` over everything). Until then the terminal shows
    /// the empty popup the first draw made with the job's output written
    /// over it (`popup_job_update_cb`), so what the job left blank on a
    /// row stays as that empty draw.
    pub redrawn: bool,
}

/// Draw `p` over `screen` (`popup_draw_cb`), clipped to the screen.
pub fn draw(p: &PopupDraw<'_>, theme: &Theme, clear: Clear, screen: &mut Snapshot) {
    let (sx, sy) = (usize::from(p.sx), usize::from(p.sy));
    let fg = theme.colour(p.defaults.fg);
    let bg = theme.colour(p.defaults.bg);
    // A content cell in the grid's colours, and the background tmux
    // compares when it looks for empty runs (`tty_draw_line_get_empty`):
    // the grid's own, before the defaults replace it (tty_attributes).
    let content = |c: Option<&Cell>| {
        let mut c = c.cloned().unwrap_or_else(|| Cell {
            width: 1,
            ..Cell::default()
        });
        let raw = grid_colour(c.style.bg);
        if c.style.fg == Color::Default {
            c.style.fg = fg;
        }
        if c.style.bg == Color::Default {
            c.style.bg = bg;
        }
        (c, raw)
    };
    let inner = |x: usize, y: usize| {
        p.content.cell(
            u16::try_from(x).unwrap_or(u16::MAX),
            u16::try_from(y).unwrap_or(u16::MAX),
        )
    };
    let boxed = |text: &str| boxed(p, text);
    let drawn = |c: &styled::Cell| (theme.cell(c), c.style.bg);
    let framed = p.lines != BoxLines::None;
    let side = p.lines.glyphs()[5];
    // The row as tty_draw_line writes it (popup_draw_cb), `inner` giving
    // the content's cells.
    let draw_row = |y: usize, with_content: bool| {
        let inner = |x: usize, y: usize| inner(x, y).filter(|_| with_content);
        let row: Vec<(Cell, Colour)> = if !framed {
            (0..sx).map(|x| content(inner(x, y))).collect()
        } else if sx <= 2 || sy <= 2 {
            // popup.c:211: too small for a box, only cleared.
            (0..sx).map(|_| content(None)).collect()
        } else if y == 0 || y == sy - 1 {
            edge_row(p, y == 0).iter().map(drawn).collect()
        } else {
            (0..sx)
                .map(|x| {
                    if x == 0 || x == sx - 1 {
                        drawn(&boxed(side))
                    } else {
                        content(inner(x - 1, y - 1))
                    }
                })
                .collect()
        };
        let (mut cells, bgs): (Vec<Cell>, Vec<Colour>) = row.into_iter().unzip();
        // tty_draw_line starts with the defaults' background as the last
        // cell's (tty-draw.c:177-178).
        draw_line_by(&mut cells, &bgs, p.defaults.bg, clear, false);
        cells
    };
    for y in 0..sy {
        let mut cells = draw_row(y, true);
        // tmux draws the empty popup once, then writes the job's output
        // into it as it comes (popup_job_update_cb): past what the job
        // wrote on a row, a bordered row stays as that draw of the empty
        // popup left it until a full redraw, and the spaces the job wrote
        // stay spaces in the colours it wrote them in (tty_cmd_cells), not
        // runs the draw would erase. Without a border an empty row has no
        // cells at all (cellsize 0, tty-draw.c:241-245), so it is erased,
        // first draw or not.
        let boxed_row = framed && sx > 2 && sy > 2 && (1..sy - 1).contains(&y);
        let first = !p.redrawn;
        if (first && boxed_row) || !framed {
            let off = usize::from(framed);
            let end = sx - off;
            let used = used_width(p.content, y - off);
            let from = (off + used).min(end);
            if framed {
                let empty = draw_row(y, false);
                cells[from..end].clone_from_slice(&empty[from..end]);
            } else {
                // The tail is its own run of empty cells: spaces in the
                // defaults' colours when short, erased from ten.
                let mut tail: Vec<Cell> = (from..end).map(|_| content(None).0).collect();
                let bgs = vec![p.defaults.bg; tail.len()];
                draw_line_by(&mut tail, &bgs, p.defaults.bg, clear, false);
                cells[from..end].clone_from_slice(&tail);
            }
            if first {
                for x in 0..used.min(end - off) {
                    if let Some(c) = inner(x, y - off).filter(|c| c.text == " " && c.width == 1) {
                        cells[off + x] = content(Some(c)).0;
                    }
                }
            }
        }
        let at_y = usize::from(p.py) + y;
        if at_y >= usize::from(screen.rows) {
            break;
        }
        let cols = usize::from(screen.cols);
        let at_x = usize::from(p.px);
        let n = sx.min(cols.saturating_sub(at_x));
        let start = at_y * cols + at_x;
        screen.cells[start..start + n].clone_from_slice(&cells[..n]);
    }
}

/// The popup's rectangle as the terminal still shows it (`held`, the
/// whole last frame): nothing is written while the tty is frozen.
pub fn keep(p: &PopupDraw<'_>, held: &[Cell], screen: &mut Snapshot) {
    let cols = usize::from(screen.cols);
    for y in
        usize::from(p.py)..(usize::from(p.py) + usize::from(p.sy)).min(usize::from(screen.rows))
    {
        let x0 = usize::from(p.px);
        let x1 = (x0 + usize::from(p.sx)).min(cols);
        if y * cols + x1 <= held.len() {
            screen.cells[y * cols + x0..y * cols + x1]
                .clone_from_slice(&held[y * cols + x0..y * cols + x1]);
        }
    }
}

/// A cell of the box: `GRID_FLAG_NOPALETTE`, it keeps its own colours.
fn boxed(p: &PopupDraw<'_>, text: &str) -> styled::Cell {
    styled::Cell {
        text: text.into(),
        width: 1,
        style: p.border.clone(),
    }
}

/// The box's top (with the title) or bottom line (`screen_write_box`).
fn edge_row(p: &PopupDraw<'_>, top: bool) -> Vec<styled::Cell> {
    let sx = usize::from(p.sx);
    let [top_left, top_right, bottom_left, bottom_right, line, _] = p.lines.glyphs();
    let (l, r) = if top {
        (top_left, top_right)
    } else {
        (bottom_left, bottom_right)
    };
    let mut row: Vec<styled::Cell> = (0..sx)
        .map(|x| {
            let glyph = if x == 0 {
                l
            } else if x == sx - 1 {
                r
            } else {
                line
            };
            boxed(p, glyph)
        })
        .collect();
    if top && !p.title.is_empty() && sx > 4 {
        // format_draw at x 2, `nx - 4` wide, in the border cell without
        // the line-drawing charset; the line shows where the title does
        // not reach.
        let under = row[2..sx - 2].to_vec();
        let title = styled::draw_onto(p.border, under, p.title, false, false);
        row.splice(2..sx - 2, title.cells);
    }
    row
}

/// How much of row `y` was written: up to its last cell that is not a
/// blank default cell (the grid line's `cellsize`; a snapshot has no such
/// count, and a space written in the default style looks the same).
fn used_width(content: &Snapshot, y: usize) -> usize {
    let Ok(y) = u16::try_from(y) else { return 0 };
    (0..content.cols)
        .rev()
        .find(|&x| {
            content.cell(x, y).is_some_and(|c| {
                !c.text.is_empty() || c.style != vtmux_vt::Style::default() || c.acs || c.link
            })
        })
        .map_or(0, |x| usize::from(x) + 1)
}

/// A terminal colour as the grid stores it, for comparing with style
/// colours.
fn grid_colour(c: Color) -> Colour {
    match c {
        Color::Default => Colour::Default,
        Color::Palette(n @ 0..=7) => Colour::Ansi(n),
        Color::Palette(n @ 8..=15) => Colour::Bright(n - 8),
        Color::Palette(n) => Colour::Palette(n),
        Color::Rgb(r, g, b) => Colour::Rgb(r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(screen: &Snapshot) -> Vec<String> {
        (0..screen.rows)
            .map(|y| {
                (0..screen.cols)
                    .map(|x| {
                        let c = screen.cell(x, y).expect("cell");
                        if c.text.is_empty() { " " } else { &c.text }.to_owned()
                    })
                    .collect()
            })
            .collect()
    }

    fn blank(cols: u16, rows: u16) -> Snapshot {
        Snapshot {
            cols,
            rows,
            cells: vec![
                Cell {
                    text: ".".into(),
                    width: 1,
                    ..Cell::default()
                };
                usize::from(cols) * usize::from(rows)
            ],
            wrapped: Vec::new(),
            cursor: None,
            cursor_visible: true,
        }
    }

    #[test]
    fn box_title_and_content() {
        let mut content = blank(4, 2);
        content.cells[0].text = "A".into();
        let mut screen = blank(10, 5);
        let style = CellStyle::default();
        let p = PopupDraw {
            px: 1,
            py: 1,
            sx: 6,
            sy: 4,
            lines: BoxLines::Rounded,
            border: &style,
            defaults: &style,
            title: "T",
            content: &content,
            redrawn: false,
        };
        draw(&p, &Theme::default(), Clear::Bce, &mut screen);
        assert_eq!(
            text(&screen),
            [
                "..........",
                ".╭─T──╮...",
                ".│A...│...",
                ".│....│...",
                ".╰────╯...",
            ]
        );
    }

    #[test]
    fn border_choices_follow_the_option() {
        for (i, name) in BoxLines::NAMES.iter().enumerate() {
            let lines = BoxLines::from_name(name).expect("choice");
            assert_eq!(lines, BoxLines::from_choice(i64::try_from(i).unwrap()));
        }
        assert_eq!(BoxLines::from_name("bogus"), None);
    }
}
