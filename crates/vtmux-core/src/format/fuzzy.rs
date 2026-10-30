//! tmux 3.8 fuzzy.c: smart-case AND terms, OR groups, scoring and display columns.
use crate::{
    style::{Align, CellStyle, Style},
    width::char_width,
};

struct Visible {
    c: char,
    align: usize,
    offset: usize,
    width: usize,
}

fn scan(text: &str) -> (Vec<Visible>, [usize; 4]) {
    let base = CellStyle::default();
    let mut style = Style::new(&base);
    let mut widths = [0usize; 4];
    let mut out = Vec::new();
    let mut at = 0;
    let mut add = |c: char, style: &Style| {
        let align = match style.align {
            Align::Default | Align::Left => 0,
            Align::Centre => 1,
            Align::Right => 2,
            Align::AbsoluteCentre => 3,
        };
        let width = usize::from(char_width(c));
        out.push(Visible {
            c,
            align,
            offset: widths[align],
            width,
        });
        widths[align] += width;
    };
    while at < text.len() {
        if text.as_bytes()[at] == b'#' {
            let count = text.as_bytes()[at..]
                .iter()
                .take_while(|&&c| c == b'#')
                .count();
            let bracket = text.as_bytes().get(at + count) == Some(&b'[');
            let hashes = if bracket {
                count / 2
            } else {
                count.div_ceil(2)
            };
            for _ in 0..hashes {
                add('#', &style);
            }
            at += count;
            if bracket && !count.is_multiple_of(2) {
                let Some(end) = super::parse::skip(text, at + 1, b"]") else {
                    break;
                };
                let _ = style.parse(&base, &text[at + 1..end]);
                at = end + 1;
            }
            continue;
        }
        let c = text[at..].chars().next().unwrap();
        at += c.len_utf8();
        if !c.is_ascii_control() {
            add(c, &style);
        }
    }
    (out, widths)
}

fn boundary(c: char) -> bool {
    " -_/.:".contains(c)
}
fn equal(a: char, b: char, fold: bool) -> bool {
    a == b || (fold && a.is_ascii() && b.is_ascii() && a.eq_ignore_ascii_case(&b))
}
fn score_positions(pos: &[usize], text: &[Visible]) -> i64 {
    let first = pos[0];
    let mut score = if first == 0 {
        12
    } else {
        i64::from(boundary(text[first - 1].c)) * 8 - i64::try_from(first.min(10)).unwrap()
    };
    for pair in pos.windows(2) {
        if pair[1] == pair[0] + 1 {
            score += 6;
        } else if boundary(text[pair[1] - 1].c) {
            score += 8;
        }
    }
    score - i64::try_from(pos[pos.len() - 1] - first + 1 - pos.len()).unwrap()
}

fn term(raw: &str, text: &[Visible], fold: bool) -> Option<(i64, Vec<usize>)> {
    let (inverse, raw) = raw.strip_prefix('!').map_or((false, raw), |s| (true, s));
    let (prefix, exact, raw) = if let Some(s) = raw.strip_prefix('\'') {
        (false, true, s)
    } else if let Some(s) = raw.strip_prefix('^') {
        (true, true, s)
    } else {
        (false, inverse, raw)
    };
    let (suffix, raw) = raw.strip_suffix('$').map_or((false, raw), |s| (true, s));
    if raw.is_empty() {
        return None;
    }
    let token: Vec<char> = raw.chars().collect();
    let found = if exact || suffix {
        let mut best = None;
        if token.len() <= text.len() {
            for start in 0..=text.len() - token.len() {
                if prefix && start != 0 || suffix && start + token.len() != text.len() {
                    continue;
                }
                if token
                    .iter()
                    .enumerate()
                    .all(|(i, &c)| equal(c, text[start + i].c, fold))
                {
                    let score = 1000
                        + i64::try_from(token.len()).unwrap() * 6
                        + i64::from(prefix) * 200
                        + i64::from(suffix) * 100
                        + if start == 0 {
                            12
                        } else {
                            i64::from(boundary(text[start - 1].c)) * 8
                        }
                        - i64::try_from(start.min(10)).unwrap()
                        - if prefix || suffix {
                            0
                        } else {
                            i64::try_from(text.len() - start - token.len()).unwrap()
                        };
                    if best.as_ref().is_none_or(|(s, _)| score > *s) {
                        best = Some((score, (start..start + token.len()).collect()));
                    }
                }
            }
        }
        best
    } else {
        let mut pos = Vec::new();
        let mut at = 0;
        for &c in &token {
            while at < text.len() && !equal(c, text[at].c, fold) {
                at += 1;
            }
            if at == text.len() {
                break;
            }
            pos.push(at);
            at += 1;
        }
        if pos.len() == token.len() {
            let mut at = pos[pos.len() - 1];
            for i in (0..token.len()).rev() {
                while !equal(token[i], text[at].c, fold) {
                    at -= 1;
                }
                pos[i] = at;
                if i != 0 {
                    at -= 1;
                }
            }
            Some((score_positions(&pos, text), pos))
        } else {
            None
        }
    };
    if inverse {
        found.is_none().then_some((0, Vec::new()))
    } else {
        found
    }
}

pub(super) fn matches(pattern: &str, text: &str, positions: bool) -> String {
    let width = crate::styled::format_width(text).max(1);
    if pattern.chars().all(|c| c == ' ' || c == '|') {
        return if positions { "" } else { "1" }.to_owned();
    }
    let fold = !pattern.chars().any(|c| c.is_ascii_uppercase());
    let (visible, widths) = scan(text);
    let mut best: Option<(i64, Vec<usize>)> = None;
    for group in pattern
        .split('|')
        .filter(|g| !g.trim_matches(' ').is_empty())
    {
        let mut score = 0;
        let mut matched = Vec::new();
        let mut yes = true;
        for raw in group.split(' ').filter(|s| !s.is_empty()) {
            if let Some((value, pos)) = term(raw, &visible, fold) {
                score += value;
                matched.extend(pos);
            } else {
                yes = false;
                break;
            }
        }
        if yes && best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, matched));
        }
    }
    let Some((_, matched)) = best else {
        return if positions { "" } else { "0" }.to_owned();
    };
    if !positions {
        return "1".to_owned();
    }
    let mut vis = widths;
    while vis[0] + vis[1] + vis[2] > width {
        if vis[1] > 0 {
            vis[1] -= 1;
        } else if vis[2] > 0 {
            vis[2] -= 1;
        } else {
            vis[0] -= 1;
        }
    }
    vis[3] = vis[3].min(width);
    let starts = [
        0,
        vis[0] + (width - vis[2] - vis[0]) / 2 - vis[1] / 2,
        width - vis[2],
        (width - vis[3]) / 2,
    ];
    let sources = [0, widths[1] / 2 - vis[1] / 2, widths[2] - vis[2], 0];
    let mut columns = std::collections::BTreeSet::new();
    for i in matched {
        let c = &visible[i];
        if c.offset < sources[c.align] || c.offset >= sources[c.align] + vis[c.align] {
            continue;
        }
        let column = starts[c.align] + c.offset - sources[c.align];
        columns.extend(column..(column + c.width).min(width));
    }
    columns
        .into_iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(",")
}
