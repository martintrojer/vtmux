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
    // Each URI's place in `out.links`.
    let mut known: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
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
        let link = if c.link.is_some() {
            let uri = link(x).unwrap_or_default();
            let at = *known.entry(uri).or_insert_with_key(|uri| {
                out.links.push(snap::Hyperlink {
                    uri: uri.clone().into_bytes(),
                    ..snap::Hyperlink::default()
                });
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

/// The vtmux-vt row of a snapshot row, and its hyperlinks by column, each
/// link the one `link` gives for its URI: the rows keep no link's id.
pub fn row_in(
    row: &snap::Row,
    link: &mut dyn FnMut(&str) -> std::sync::Arc<vt::Link>,
) -> (vt::Row, Vec<(usize, String)>) {
    let mut links = Vec::new();
    // Each column's grapheme tail: the first one listed for it.
    let mut rests: Vec<Option<&str>> = vec![None; row.cells.len()];
    for g in &row.graphemes {
        if let Some(slot @ None) = rests.get_mut(usize::from(g.col)) {
            *slot = Some(&g.rest);
        }
    }
    let cells = row
        .cells
        .iter()
        .enumerate()
        .map(|(x, c)| {
            let mut text = char::from_u32(c.codepoint)
                .filter(|_| c.codepoint != 0)
                .map(String::from)
                .unwrap_or_default();
            if let Some(rest) = rests[x] {
                text.push_str(rest);
            }
            let (width, tab) = match (c.tab, c.wide) {
                (snap::Tab::Start(n), _) => (n, true),
                (snap::Tab::Covered, _) | (snap::Tab::None, snap::Wide::SpacerTail) => (0, false),
                (snap::Tab::None, snap::Wide::Wide) => (2, false),
                (snap::Tab::None, _) => (1, false),
            };
            let mut linked = None;
            if c.link != 0
                && let Some(l) = row.links.get(usize::from(c.link) - 1)
                && !l.uri.is_empty()
            {
                // Copy mode keeps a link's URI as text (`vt::Rows`), and
                // wrote it so.
                let uri = String::from_utf8_lossy(&l.uri).into_owned();
                linked = Some(link(&uri));
                links.push((x, uri));
            }
            vt::Cell {
                text: text.into(),
                width,
                style: style_in(&c.style),
                acs: c.flags & snap::row::cell_flag::ACS != 0,
                link: linked,
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
        let link = vt::Link::fresh("http://x");
        linked.link = Some(std::sync::Arc::clone(&link));
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
        let (back, links) = row_in(&out, &mut |uri| {
            assert_eq!(uri, "http://x");
            std::sync::Arc::clone(&link)
        });
        assert_eq!(back, row);
        assert_eq!(links, [(5, "http://x".to_owned())]);
    }

    /// Graphemes in any order, the first listed for a column winning, and
    /// one hyperlink entry per URI however many cells carry it.
    #[test]
    fn graphemes_and_links_are_looked_up_by_column_and_uri() {
        let mut row = snap::Row {
            cells: vec![snap::Cell::default(); 3],
            ..snap::Row::default()
        };
        for c in &mut row.cells {
            c.codepoint = u32::from('e');
        }
        row.graphemes = vec![
            snap::Grapheme {
                col: 2,
                rest: "\u{301}".into(),
            },
            snap::Grapheme {
                col: 0,
                rest: "\u{300}".into(),
            },
            snap::Grapheme {
                col: 2,
                rest: "\u{302}".into(),
            },
            snap::Grapheme {
                col: 9,
                rest: "\u{303}".into(),
            },
        ];
        let (back, _) = row_in(&row, &mut |uri| vt::Link::fresh(uri));
        let texts: Vec<&str> = back.cells.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["e\u{300}", "e", "e\u{301}"]);

        let linked = |text: &str| vt::Cell {
            text: text.into(),
            width: 1,
            link: Some(vt::Link::fresh("http://l")),
            ..vt::Cell::default()
        };
        let row = vt::Row {
            cells: vec![linked("a"), linked("b"), linked("c"), linked("d")],
            wrapped: false,
        };
        let uris = ["http://a", "http://b", "http://a", "http://b"];
        let out = row_out(&row, |x| Some(uris[x].to_owned()));
        let refs: Vec<u16> = out.cells.iter().map(|c| c.link).collect();
        assert_eq!(refs, [1, 2, 1, 2]);
        assert_eq!(out.links.len(), 2);
    }
}
