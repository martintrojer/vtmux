//! Width and truncation of expanded text that may still hold `#[style]`
//! runs and `##` escapes: `format_width`, `format_trim_left` and
//! `format_trim_right` (format-draw.c:1100-1270), used by the `=` modifier
//! and the status line.
//!
//! Styles take no width and are always kept. A run of `n` `#`s is
//! `ceil(n/2)` columns (the drawing code halves it later); before `[` an odd
//! run ends in a style. Characters other than printable ASCII and valid
//! UTF-8 (C0 controls, DEL) are dropped, as tmux drops them.

use unicode_width::UnicodeWidthChar;

use super::parse::skip;

/// A run of `#`s at `s[i..]`: (count, width, index just past what the caller
/// may skip). For an odd run before `[` the index points at the last `#`,
/// which starts the style (`format_leading_hashes`, format-draw.c:646-674).
fn leading_hashes(s: &[u8], i: usize) -> (usize, usize, usize) {
    let n = s[i..].iter().take_while(|&&c| c == b'#').count();
    if s.get(i + n) != Some(&b'[') {
        return (n, n.div_ceil(2), i + n);
    }
    let end = if n % 2 == 0 { i + n } else { i + n - 1 };
    (n, n / 2, end)
}

/// One step of the scan: a hash run, a style, a character or a dropped byte.
enum Piece<'a> {
    /// `count` `#`s worth `width` columns.
    Hashes {
        count: usize,
        width: usize,
    },
    Style(&'a str),
    Char(&'a str, usize),
    Skip,
    /// An unterminated `#[`: scanning stops.
    Broken,
}

struct Scan<'a> {
    s: &'a str,
    i: usize,
    /// The previous piece was a hash run that ended at a style's `#`.
    style_next: bool,
}

impl<'a> Scan<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            s,
            i: 0,
            style_next: false,
        }
    }
}

impl<'a> Iterator for Scan<'a> {
    type Item = Piece<'a>;

    fn next(&mut self) -> Option<Piece<'a>> {
        let b = self.s.as_bytes();
        if self.i >= b.len() {
            return None;
        }
        if std::mem::take(&mut self.style_next) {
            // At the `#` of `#[`: copy through the matching `]`.
            let Some(close) = skip(self.s, self.i + 2, b"]") else {
                self.i = b.len();
                return Some(Piece::Broken);
            };
            let style = &self.s[self.i..=close];
            self.i = close + 1;
            return Some(Piece::Style(style));
        }
        if b[self.i] == b'#' {
            let (count, width, end) = leading_hashes(b, self.i);
            self.i = end;
            self.style_next = b.get(end) == Some(&b'#');
            return Some(Piece::Hashes { count, width });
        }
        let c = self.s[self.i..].chars().next().expect("in bounds");
        let len = c.len_utf8();
        let text = &self.s[self.i..self.i + len];
        self.i += len;
        if c.is_ascii() {
            if (' '..='~').contains(&c) {
                return Some(Piece::Char(text, 1));
            }
            return Some(Piece::Skip);
        }
        Some(Piece::Char(text, c.width().unwrap_or(0)))
    }
}

/// Copy a hash run of `width` columns as format-draw.c does: a lone `#`
/// stays one, a longer run becomes `2 * width` `#`s.
fn push_hashes(out: &mut String, count: usize, width: usize) {
    if width == 0 {
        return;
    }
    if count == 1 {
        out.push('#');
    } else {
        out.extend(std::iter::repeat_n('#', 2 * width));
    }
}

/// `format_width`: display columns, styles excluded. An unterminated
/// style makes the whole text zero wide.
#[must_use]
pub fn width(s: &str) -> usize {
    let mut w = 0;
    for piece in Scan::new(s) {
        match piece {
            Piece::Hashes { width, .. } => w += width,
            Piece::Char(_, cw) => w += cw,
            Piece::Broken => return 0,
            Piece::Style(_) | Piece::Skip => {}
        }
    }
    w
}

/// `format_trim_left`: keep the first `limit` columns.
#[must_use]
pub fn trim_left(s: &str, limit: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for piece in Scan::new(s) {
        if w >= limit {
            break;
        }
        match piece {
            Piece::Hashes { count, width } => {
                let width = width.min(limit - w);
                push_hashes(&mut out, count, width);
                w += width;
            }
            Piece::Style(style) => out.push_str(style),
            Piece::Char(text, cw) => {
                if w + cw <= limit {
                    out.push_str(text);
                }
                w += cw;
            }
            Piece::Broken => break,
            Piece::Skip => {}
        }
    }
    out
}

/// `format_trim_right`: keep the last `limit` columns.
#[must_use]
pub fn trim_right(s: &str, limit: usize) -> String {
    let total = width(s);
    if total <= limit {
        return s.to_owned();
    }
    let skip = total - limit;
    let mut out = String::new();
    let mut w = 0;
    for piece in Scan::new(s) {
        match piece {
            Piece::Hashes { count, width } => {
                let mut copy = width;
                if w <= skip {
                    copy = copy.saturating_sub(skip - w);
                }
                push_hashes(&mut out, count, copy);
                w += width;
            }
            Piece::Style(style) => out.push_str(style),
            Piece::Char(text, cw) => {
                if w >= skip {
                    out.push_str(text);
                }
                w += cw;
            }
            Piece::Broken => break,
            Piece::Skip => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths() {
        assert_eq!(width("hello"), 5);
        assert_eq!(width("#[fg=red]ab#[default]"), 2);
        assert_eq!(width("a##b"), 3);
        assert_eq!(width("a#b"), 3);
        assert_eq!(width("a世b"), 4);
        assert_eq!(width("a\x01b"), 2);
        assert_eq!(width("ab#[fg=red"), 0);
    }

    #[test]
    fn trims() {
        assert_eq!(trim_left("hello", 3), "hel");
        assert_eq!(trim_right("hello", 3), "llo");
        assert_eq!(trim_left("hello", 10), "hello");
        assert_eq!(trim_left("#[fg=red]abc", 2), "#[fg=red]ab");
        assert_eq!(trim_left("a世b", 2), "a");
        assert_eq!(trim_right("a世b", 2), "b");
        assert_eq!(trim_left("a\x01bc", 2), "ab");
    }
}
