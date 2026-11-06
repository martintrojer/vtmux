//! vtmux-vt rows (copy mode's frozen grid) to the snapshot's row format
//! and back (D27 decision 11).

use vtmux_snapshot as snap;
use vtmux_vt as vt;

fn colour_out(c: vt::Color) -> snap::Color {
    match c {
        vt::Color::Default => snap::Color::Default,
        vt::Color::Palette(n) => snap::Color::Palette(n),
        vt::Color::Palette256(n) => snap::Color::Palette256(n),
        vt::Color::Rgb(r, g, b) => snap::Color::Rgb(r, g, b),
    }
}

fn colour_in(c: snap::Color) -> vt::Color {
    match c {
        snap::Color::Default => vt::Color::Default,
        snap::Color::Palette(n) => vt::Color::Palette(n),
        snap::Color::Palette256(n) => vt::Color::Palette256(n),
        snap::Color::Rgb(r, g, b) => vt::Color::Rgb(r, g, b),
    }
}

const ATTRS: [u16; 8] = [
    snap::row::attr::BOLD,
    snap::row::attr::FAINT,
    snap::row::attr::ITALIC,
    snap::row::attr::BLINK,
    snap::row::attr::INVERSE,
    snap::row::attr::INVISIBLE,
    snap::row::attr::STRIKETHROUGH,
    snap::row::attr::OVERLINE,
];

fn style_out(s: &vt::Style) -> snap::Style {
    let on = [
        s.bold,
        s.faint,
        s.italic,
        s.blink,
        s.inverse,
        s.invisible,
        s.strikethrough,
        s.overline,
    ];
    snap::Style {
        fg: colour_out(s.fg),
        bg: colour_out(s.bg),
        underline_color: colour_out(s.underline_color),
        attrs: ATTRS
            .iter()
            .zip(on)
            .filter(|(_, on)| *on)
            .fold(0, |a, (bit, _)| a | bit),
        underline: match s.underline {
            vt::Underline::None => snap::Underline::None,
            vt::Underline::Single => snap::Underline::Single,
            vt::Underline::Double => snap::Underline::Double,
            vt::Underline::Curly => snap::Underline::Curly,
            vt::Underline::Dotted => snap::Underline::Dotted,
            vt::Underline::Dashed => snap::Underline::Dashed,
        },
    }
}

fn style_in(s: &snap::Style) -> vt::Style {
    let has = |bit: u16| s.attrs & bit != 0;
    vt::Style {
        fg: colour_in(s.fg),
        bg: colour_in(s.bg),
        underline_color: colour_in(s.underline_color),
        bold: has(snap::row::attr::BOLD),
        faint: has(snap::row::attr::FAINT),
        italic: has(snap::row::attr::ITALIC),
        blink: has(snap::row::attr::BLINK),
        inverse: has(snap::row::attr::INVERSE),
        invisible: has(snap::row::attr::INVISIBLE),
        strikethrough: has(snap::row::attr::STRIKETHROUGH),
        overline: has(snap::row::attr::OVERLINE),
        underline: match s.underline {
            snap::Underline::None => vt::Underline::None,
            snap::Underline::Single => vt::Underline::Single,
            snap::Underline::Double => vt::Underline::Double,
            snap::Underline::Curly => vt::Underline::Curly,
            snap::Underline::Dotted => vt::Underline::Dotted,
            snap::Underline::Dashed => vt::Underline::Dashed,
        },
    }
}

/// A row of used cells as the snapshot's row; `link(x)` is the hyperlink
/// of a linked cell (`None`: linked, but the URI is not known).
pub fn row_out(row: &vt::Row, link: impl Fn(usize) -> Option<String>) -> snap::Row {
    let mut out = snap::Row {
        wrapped: row.wrapped,
        cells_used: u16::try_from(row.cells.len()).unwrap_or(u16::MAX),
        ..snap::Row::default()
    };
    // Columns still covered by the last tab.
    let mut tab_left = 0usize;
    for (x, c) in row.cells.iter().enumerate() {
        let mut chars = c.text.as_str().chars();
        let codepoint = chars.next().map_or(0, u32::from);
        let rest: String = chars.collect();
        let (wide, tab) = if c.tab {
            tab_left = usize::from(c.width).saturating_sub(1);
            (snap::Wide::Narrow, snap::Tab::Start(c.width))
        } else if c.width == 0 && tab_left > 0 {
            tab_left -= 1;
            (snap::Wide::Narrow, snap::Tab::Covered)
        } else {
            tab_left = 0;
            match c.width {
                0 => (snap::Wide::SpacerTail, snap::Tab::None),
                2 => (snap::Wide::Wide, snap::Tab::None),
                _ => (snap::Wide::Narrow, snap::Tab::None),
            }
        };
        let link = if c.link {
            let uri = link(x).unwrap_or_default();
            let at = out
                .links
                .iter()
                .position(|l| l.uri == uri)
                .unwrap_or_else(|| {
                    out.links.push(snap::Hyperlink { id: None, uri });
                    out.links.len() - 1
                });
            u16::try_from(at + 1).unwrap_or(0)
        } else {
            0
        };
        if !rest.is_empty() {
            out.graphemes.push(snap::Grapheme {
                col: u16::try_from(x).unwrap_or(u16::MAX),
                rest,
            });
        }
        out.cells.push(snap::Cell {
            codepoint,
            wide,
            style: style_out(&c.style),
            flags: if c.acs { snap::row::cell_flag::ACS } else { 0 },
            link,
            semantic: snap::Semantic::Output,
            tab,
        });
    }
    out
}

/// The vtmux-vt row of a snapshot row, and its hyperlinks by column.
pub fn row_in(row: &snap::Row) -> (vt::Row, Vec<(usize, String)>) {
    let mut links = Vec::new();
    let cells = row
        .cells
        .iter()
        .enumerate()
        .map(|(x, c)| {
            let mut text = char::from_u32(c.codepoint)
                .filter(|_| c.codepoint != 0)
                .map(String::from)
                .unwrap_or_default();
            if let Some(g) = row.graphemes.iter().find(|g| usize::from(g.col) == x) {
                text.push_str(&g.rest);
            }
            let (width, tab) = match (c.tab, c.wide) {
                (snap::Tab::Start(n), _) => (n, true),
                (snap::Tab::Covered, _) | (snap::Tab::None, snap::Wide::SpacerTail) => (0, false),
                (snap::Tab::None, snap::Wide::Wide) => (2, false),
                (snap::Tab::None, _) => (1, false),
            };
            if c.link != 0
                && let Some(l) = row.links.get(usize::from(c.link) - 1)
                && !l.uri.is_empty()
            {
                links.push((x, l.uri.clone()));
            }
            vt::Cell {
                text: text.into(),
                width,
                style: style_in(&c.style),
                acs: c.flags & snap::row::cell_flag::ACS != 0,
                link: c.link != 0,
                tab,
            }
        })
        .collect();
    (
        vt::Row {
            cells,
            wrapped: row.wrapped,
        },
        links,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_survive_the_snapshot_format() {
        let cell = |text: &str, width: u8| vt::Cell {
            text: text.into(),
            width,
            ..vt::Cell::default()
        };
        let mut styled = cell("b", 1);
        styled.style.bold = true;
        styled.style.fg = vt::Color::Palette256(3);
        styled.style.underline = vt::Underline::Curly;
        styled.acs = true;
        let mut linked = cell("l", 1);
        linked.link = true;
        let mut tab = cell("   ", 3);
        tab.tab = true;
        let row = vt::Row {
            cells: vec![
                cell("a", 1),
                cell("漢", 2),
                cell("", 0),
                styled,
                cell("e\u{301}", 1),
                linked,
                tab,
                cell("", 0),
                cell("", 0),
                cell("", 1),
            ],
            wrapped: true,
        };
        let out = row_out(&row, |x| (x == 5).then(|| "http://x".to_owned()));
        let (back, links) = row_in(&out);
        assert_eq!(back, row);
        assert_eq!(links, [(5, "http://x".to_owned())]);
    }
}
