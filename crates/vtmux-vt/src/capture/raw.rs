//! `capture-pane -R` (cmd-capture-pane.c `cmd_capture_pane_grid`): a dump
//! of the grid for diagnostics, one `G` line, an `L` line per row (history
//! first) and a `C` line per cell, in tmux's words.
//!
//! The cells are Ghostty's read back in tmux's shapes, so where tmux's grid
//! keeps something Ghostty does not, the dump says what can be told:
//! - `cellused`/`cellsize` and `CLEARED` cells follow the capture's
//!   allocation model ([`super::tmux_cellsize`]): cells past the last
//!   written one up to the allocated size are `CLEARED`, the rest are
//!   default cells;
//! - a palette colour 0-7 is the named colour, 8-15 the bright one, the rest
//!   `colourN` (Ghostty keeps one index where tmux tells `31` from
//!   `38;5;1`, as `capture-pane -e` does);
//! - an ACS cell shows the character that was printed (Ghostty's line
//!   drawing table read backwards), as tmux stores it;
//! - a link's `id=` comes from Ghostty patch 0006; one opened without an
//!   id is `NONE`;
//! - `EXTENDED` and `HYPERLINK` are set when a cell on the line needs
//!   them now or the row kept them (Ghostty patch 0034: tmux keeps them
//!   once a cell needed the extended form, `super::LineKept`);
//! - OSC 133 line data (the flags each kind sets, the prompt, command and
//!   output columns, the exit status) is kept by [`PaneTerm`] per line, as
//!   `input_osc_133` keeps it.

use std::fmt::Write as _;

use super::{PaneRows, RowCell, Width, tmux_cellsize};
use crate::snapshot::{Color, Style, Underline};
use crate::term::{Osc133, PaneTerm};

/// `GRID_LINE_*` names in `grid_line_flags_string` order.
fn line_flags(wrapped: bool, extended: bool, osc133: u32, link: bool) -> (String, u32) {
    let mut names = Vec::new();
    let mut bits = 0;
    let mut add = |on: bool, name: &'static str, bit: u32| {
        if on {
            names.push(name);
            bits |= bit;
        }
    };
    add(wrapped, "WRAPPED", 0x1);
    add(extended, "EXTENDED", 0x2);
    add(osc133 & 0x8 != 0, "START_PROMPT", 0x8);
    add(osc133 & 0x10 != 0, "SECOND_PROMPT", 0x10);
    add(osc133 & 0x20 != 0, "START_COMMAND", 0x20);
    add(osc133 & 0x40 != 0, "START_OUTPUT", 0x40);
    add(osc133 & 0x80 != 0, "END_OUTPUT", 0x80);
    add(link, "HYPERLINK", 0x100);
    (joined(&names), bits)
}

fn joined(names: &[&str]) -> String {
    if names.is_empty() {
        "NONE".to_owned()
    } else {
        names.join(",")
    }
}

/// `colour_tostring` and the colour's number as tmux stores it.
fn colour(c: Color) -> (String, u32) {
    const NAMES: [&str; 8] = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    ];
    match c {
        Color::Default => ("default".to_owned(), 8),
        Color::Palette(i) if i < 8 => (NAMES[usize::from(i)].to_owned(), u32::from(i)),
        Color::Palette(i) if i < 16 => (
            format!("bright{}", NAMES[usize::from(i - 8)]),
            90 + u32::from(i - 8),
        ),
        Color::Palette(i) | Color::Palette256(i) => {
            (format!("colour{i}"), 0x0100_0000 | u32::from(i))
        }
        Color::Rgb(r, g, b) => (
            format!("#{r:02x}{g:02x}{b:02x}"),
            0x0200_0000 | u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b),
        ),
    }
}

/// `grid_cell_attr_string` and the `GRID_ATTR_*` bits.
fn attrs(s: &Style, acs: bool) -> (String, u32) {
    let mut names = Vec::new();
    let mut bits = 0;
    let mut add = |on: bool, name: &'static str, bit: u32| {
        if on {
            names.push(name);
            bits |= bit;
        }
    };
    add(acs, "CHARSET", 0x80);
    add(s.bold, "BRIGHT", 0x1);
    add(s.faint, "DIM", 0x2);
    add(s.underline == Underline::Single, "UNDERSCORE", 0x4);
    add(s.blink, "BLINK", 0x8);
    add(s.inverse, "REVERSE", 0x10);
    add(s.invisible, "HIDDEN", 0x20);
    add(s.italic, "ITALICS", 0x40);
    add(s.strikethrough, "STRIKETHROUGH", 0x100);
    add(s.underline == Underline::Double, "UNDERSCORE_2", 0x200);
    add(s.underline == Underline::Curly, "UNDERSCORE_3", 0x400);
    add(s.underline == Underline::Dotted, "UNDERSCORE_4", 0x800);
    add(s.underline == Underline::Dashed, "UNDERSCORE_5", 0x1000);
    add(s.overline, "OVERLINE", 0x2000);
    (joined(&names), bits)
}

/// `utf8_stravis(VIS_OCTAL|VIS_CSTYLE|VIS_TAB|VIS_NL)` of a cell's data:
/// controls and backslash escaped, the rest as it is.
fn vis(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x0b' => out.push_str("\\v"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\{:03o}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// What one cell of tmux's grid holds: width, data and the cell flags.
struct Raw {
    width: usize,
    data: String,
    padding: bool,
    cleared: bool,
    tab: bool,
}

/// Ghostty's DEC Special Graphics table (charsets.zig `dec_special`)
/// backwards: the character the program printed for a glyph, as tmux
/// stores it (the charset is an attribute in tmux).
pub(crate) fn dec_special_source(c: char) -> char {
    const TABLE: [(u32, char); 31] = [
        (0x25C6, '`'),
        (0x2592, 'a'),
        (0x2409, 'b'),
        (0x240C, 'c'),
        (0x240D, 'd'),
        (0x240A, 'e'),
        (0x00B0, 'f'),
        (0x00B1, 'g'),
        (0x2424, 'h'),
        (0x240B, 'i'),
        (0x2518, 'j'),
        (0x2510, 'k'),
        (0x250C, 'l'),
        (0x2514, 'm'),
        (0x253C, 'n'),
        (0x23BA, 'o'),
        (0x23BB, 'p'),
        (0x2500, 'q'),
        (0x23BC, 'r'),
        (0x23BD, 's'),
        (0x251C, 't'),
        (0x2524, 'u'),
        (0x2534, 'v'),
        (0x252C, 'w'),
        (0x2502, 'x'),
        (0x2264, 'y'),
        (0x2265, 'z'),
        (0x03C0, '{'),
        (0x2260, '|'),
        (0x00A3, '}'),
        (0x00B7, '~'),
    ];
    TABLE
        .iter()
        .find(|(g, _)| *g == c as u32)
        .map_or(c, |(_, s)| *s)
}

/// An empty cell inside the line's allocated cells is a `CLEARED` one:
/// tmux clears the cells it allocates (`grid_expand_line`) and those an
/// erase reaches; past the allocation the cell is the default one. One
/// cleared to an RGB background is an extended cell, which keeps no
/// `CLEARED` (`grid_clear_cell`, `grid_extended_cell`).
fn raw(c: &RowCell, in_size: bool) -> Raw {
    if c.padding() && !matches!(c.tab, 1..=32) {
        return Raw {
            width: 1,
            data: "!".to_owned(),
            padding: true,
            cleared: false,
            tab: false,
        };
    }
    if matches!(c.tab, 1..=32) {
        return Raw {
            width: usize::from(c.tab),
            data: " ".repeat(usize::from(c.tab)),
            padding: false,
            cleared: false,
            tab: true,
        };
    }
    if c.empty {
        return Raw {
            width: 1,
            data: " ".to_owned(),
            padding: false,
            cleared: !matches!(c.style.bg, Color::Rgb(..))
                && (in_size || c.style.bg != Color::Default),
            tab: false,
        };
    }
    Raw {
        width: if c.wide == Width::Double { 2 } else { 1 },
        data: c.data().into_owned(),
        padding: false,
        cleared: false,
        tab: false,
    }
}

/// `capture-pane -R`: the dump, with its final newline.
#[must_use]
pub fn capture_grid(term: &PaneTerm, history_limit: usize) -> Vec<u8> {
    let p = PaneRows::of(term);
    let (sy, sx, hsize) = (p.rows, p.cols, p.hsize);
    let osc133 = term.osc133_lines();
    let mut out = format!("G {sx}x{sy} ({hsize}/{history_limit})\n");
    let mut last = None;
    for y in 0..hsize + sy {
        let (cells, wrapped) = p.row(term, y, &mut last);
        // `cellused`: padding and a tab's columns are cells tmux wrote.
        let used = cells
            .iter()
            .rposition(|c| !c.empty || c.padding())
            .map_or(0, |i| i + 1);
        let size = tmux_cellsize(&cells, used).max(used);
        let kept = p.kept(term, y, &last);
        let link = kept.hyperlink || cells.iter().any(|c| c.link);
        let extended = kept.extended || super::row_extended(&cells);
        let data: Osc133 = osc133
            .iter()
            .find(|(row, _)| *row == y)
            .map(|(_, d)| *d)
            .unwrap_or_default();
        let (names, bits) = line_flags(wrapped, extended, data.flags, link);
        let p = if y < hsize {
            "-".to_owned()
        } else {
            (y - hsize).to_string()
        };
        let _ = write!(out, "\tL {y} ({p}) flags={names}[{bits:x}] {used}/{size}");
        // cmd-capture-pane.c:151-158: the OSC 133 data on a line it touched.
        if data.flags != 0 {
            let _ = write!(
                out,
                " osc133={},{},{},{},{}",
                data.prompt_col,
                data.cmd_col,
                data.out_start_col,
                data.out_end_col,
                data.exit_status
            );
        }
        out.push('\n');
        for (x, c) in cells.iter().enumerate() {
            let r = raw(c, x < size);
            let style = if r.padding { Style::default() } else { c.style };
            let (fg, fgn) = colour(style.fg);
            let (bg, bgn) = colour(style.bg);
            let (us, usn) = colour(style.underline_color);
            let mut flags = Vec::new();
            let mut fbits = 0;
            if fgn & 0x0100_0000 != 0 {
                flags.push("FG256");
                fbits |= 0x1;
            }
            if bgn & 0x0100_0000 != 0 {
                flags.push("BG256");
                fbits |= 0x2;
            }
            if r.padding {
                flags.push("PADDING");
                fbits |= 0x4;
            }
            if r.cleared {
                flags.push("CLEARED");
                fbits |= 0x40;
            }
            if r.tab {
                flags.push("TAB");
                fbits |= 0x80;
            }
            let (an, ab) = attrs(&style, c.acs && !r.padding);
            let link = c.uri().filter(|_| !r.padding).unwrap_or("NONE");
            let linkid = match (c.link_id(), r.padding, link) {
                (_, _, "NONE") | (_, true, _) => "NONE".to_owned(),
                (Some(libghostty_vt::screen::HyperlinkId::Explicit(id)), ..) if !id.is_empty() => {
                    String::from_utf8_lossy(id).into_owned()
                }
                _ => "NONE".to_owned(),
            };
            let _ = writeln!(
                out,
                "\t\tC {y},{x} data=({},{},{}) flags={}[{fbits:x}] attr={an}[{ab:x}] \
                 fg={fg}[{fgn:x}] bg={bg}[{bgn:x}] us={us}[{usn:x}] link={link} linkid={linkid}",
                r.width,
                r.data.len(),
                vis(&r.data),
                joined(&flags),
            );
        }
    }
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dump(cols: u16, rows: u16, bytes: &[u8]) -> String {
        let mut t = PaneTerm::new(cols, rows, 100);
        t.feed(bytes);
        String::from_utf8(capture_grid(&t, 100)).unwrap()
    }

    #[test]
    fn cells_flags_and_colours() {
        let d = dump(
            6,
            3,
            "a\x1b[44mb\x1b[K\x1b[m\r\n\x1b[1;38;5;196mX\x1b[m\t\u{4e16}".as_bytes(),
        );
        assert!(d.starts_with("G 6x3 (0/100)\n"), "{d}");
        assert!(
            d.contains(
                "\t\tC 0,1 data=(1,1,b) flags=NONE[0] attr=NONE[0] fg=default[8] bg=blue[4]"
            ),
            "{d}"
        );
        assert!(
            d.contains(
                "\t\tC 0,2 data=(1,1, ) flags=CLEARED[40] attr=NONE[0] fg=default[8] bg=blue[4]"
            ),
            "{d}"
        );
        assert!(
            d.contains(
                "\t\tC 1,0 data=(1,1,X) flags=FG256[1] attr=BRIGHT[1] fg=colour196[10000c4]"
            ),
            "{d}"
        );
        assert!(d.contains("\t\tC 1,1 data=(4,4,    ) flags=TAB[80]"), "{d}");
        assert!(d.contains("\t\tC 1,2 data=(1,1,!) flags=PADDING[4]"), "{d}");
        assert!(d.contains("\tL 1 (1) flags=WRAPPED,EXTENDED[3]"), "{d}");
        assert!(
            d.contains("\t\tC 2,0 data=(2,3,\u{4e16}) flags=NONE[0]"),
            "{d}"
        );
        assert!(d.contains("\t\tC 2,1 data=(1,1,!) flags=PADDING[4]"), "{d}");
    }

    /// tmux writes a wide or non-ASCII character and a tab one cell at a
    /// time, its padding first (`screen_write_cell`), so the line grows in
    /// smaller steps than for collected ASCII (`grid_expand_line`; values
    /// from tmux 3.8).
    #[test]
    fn cells_written_alone_grow_the_line_a_cell_at_a_time() {
        let line = |cols, bytes: &str| {
            dump(cols, 3, bytes.as_bytes())
                .lines()
                .nth(1)
                .unwrap()
                .to_owned()
        };
        assert_eq!(line(8, "\u{e9}a"), "\tL 0 (0) flags=EXTENDED[2] 2/2");
        assert_eq!(
            line(8, "\u{3042}\u{3042}"),
            "\tL 0 (0) flags=EXTENDED[2] 4/4"
        );
        assert_eq!(line(20, "a\tbc"), "\tL 0 (0) flags=EXTENDED[2] 10/10");
        assert_eq!(line(8, "abcd"), "\tL 0 (0) flags=NONE[0] 4/8");
    }

    /// A cell erased to an RGB background is an extended one in tmux and
    /// keeps no `CLEARED` (grid.c `grid_clear_cell`).
    #[test]
    fn an_rgb_erase_is_not_cleared() {
        let d = dump(10, 3, b"\x1b[48;2;1;2;3m\x1b[K\x1b[mab");
        assert!(
            d.contains("\t\tC 0,2 data=(1,1, ) flags=NONE[0] attr=NONE[0] fg=default[8] bg=#010203[2010203]"),
            "{d}"
        );
    }
}
