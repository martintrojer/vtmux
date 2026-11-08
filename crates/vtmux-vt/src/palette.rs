//! The colour and clipboard OSCs tmux reads itself (input.c `input_osc_4`,
//! `_10`, `_11`, `_12`, `_52`, `_104`, `_110`, `_111`, `_112`).
//!
//! The pane's palette is kept as tmux keeps it (`struct colour_palette`
//! and `s->ccolour`), so the queries are answered from it; Ghostty keeps
//! its own copy for drawing. A change calls `screen_write_fullredraw`,
//! which sets `PANE_REDRAW`; a passthrough payload later in the same input
//! is then dropped under `allow-passthrough on` (screen-write.c:150).
//! What only the server knows (a client's colours, the window style, the
//! paste buffers, the clipboard) becomes a [`Reply`] it answers.

/// An RGB colour, as `colour_split_rgb` gives it.
pub type Rgb = (u8, u8, u8);

/// `colour_byname` stand-in: the X11 name table lives in `vtmux-core`.
pub type NameCheck = fn(&str) -> Option<Rgb>;

/// How the OSC being answered ended (`ictx->input_end`): a reply ends the
/// same way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OscEnd {
    /// BEL.
    Bel,
    /// ST (`ESC \`), or anything else that ended it.
    St,
}

impl OscEnd {
    #[must_use]
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Self::Bel => b"\x07",
            Self::St => b"\x1b\\",
        }
    }
}

/// What the pane program's input asks of the server, in input order with
/// the replies the terminal gives itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// Bytes for the program (`input_reply`).
    Bytes(Vec<u8>),
    /// `OSC 10 ; ?` (`input_osc_10`): the pane's default foreground.
    Foreground(OscEnd),
    /// `OSC 11 ; ?` (`input_osc_11`): the pane's default background.
    Background(OscEnd),
    /// `OSC 12 ; ?` (`input_osc_12`): the cursor colour the program set,
    /// else the `cursor-colour` option's.
    Cursor { colour: Option<Rgb>, end: OscEnd },
    /// `OSC 4 ; n ; ?` for an entry the program did not set: the
    /// `pane-colours` entry, else a request to the client terminal.
    Palette { index: u8, end: OscEnd },
    /// `OSC 52 ; clip ; ?` (`input_osc_52_parse`): the clipboard names
    /// given (only those tmux allows, once each).
    Clipboard { clip: Vec<u8>, end: OscEnd },
    /// `OSC 52 ; clip ; base64`: set the clipboard.
    SetClipboard { clip: Vec<u8>, data: Vec<u8> },
    /// `CSI ? 996 n` (`input_report_current_theme`).
    Theme,
    /// `CSI ? 2031 h` or `l` (`MODE_THEME_UPDATES`, input.c:1985-1989,
    /// 2083-2088): theme reports on or off.
    ThemeUpdates(bool),
}

/// `input_osc_colour_reply` (input.c:2898-2924): `OSC n ; [index ;]
/// rgb:rrrr/gggg/bbbb` and the query's end.
#[must_use]
pub fn colour_reply(n: u32, index: Option<u8>, (r, g, b): Rgb, end: OscEnd) -> Vec<u8> {
    let index = index.map(|i| format!("{i};")).unwrap_or_default();
    let mut out =
        format!("\x1b]{n};{index}rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}").into_bytes();
    out.extend_from_slice(end.bytes());
    out
}

/// tmux's per-pane `colour_palette` (the entries OSC 4 set, `fg` and `bg`
/// from OSC 10 and 11) and the screen's `ccolour` (OSC 12).
#[derive(Debug)]
pub(crate) struct Palette {
    /// `palette->palette`, allocated by the first entry set.
    entries: Option<Box<[Option<Rgb>; 256]>>,
    /// `palette->fg` and `palette->bg`; `None` is tmux's 8 (default).
    pub(crate) fg: Option<Rgb>,
    pub(crate) bg: Option<Rgb>,
    /// `s->ccolour`; `None` is -1.
    cursor: Option<Rgb>,
    names: NameCheck,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            entries: None,
            fg: None,
            bg: None,
            cursor: None,
            names: |_| None,
        }
    }
}

/// Whether an OSC with this payload starts like one tmux reads here, or
/// the progress bar's OSC 9 (the full payload is worth keeping).
pub(crate) fn is_palette_osc(prefix: &[u8]) -> bool {
    matches!(
        osc_number(prefix),
        Some((4 | 9 | 10 | 11 | 12 | 52 | 104 | 110 | 111 | 112, _))
    )
}

/// `input_exit_osc`'s split: the number (a `u_int`, wrapping) and the rest
/// after its `;`.
pub(crate) fn osc_number(p: &[u8]) -> Option<(u32, &[u8])> {
    let digits = p.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let n = p[..digits].iter().fold(0u32, |a, b| {
        a.wrapping_mul(10).wrapping_add(u32::from(b - b'0'))
    });
    match p.get(digits) {
        None => Some((n, &[])),
        Some(b';') => Some((n, &p[digits + 1..])),
        Some(_) => None,
    }
}

/// tmux's palette as [`crate::TermState`] carries it: the entries OSC 4
/// set, and the colours OSC 10, 11 and 12 set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaletteState {
    pub entries: Vec<(u8, Rgb)>,
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub cursor: Option<Rgb>,
}

impl Palette {
    pub(crate) fn state(&self) -> PaletteState {
        let entries = self.entries.as_ref().map_or_else(Vec::new, |e| {
            e.iter()
                .enumerate()
                .filter_map(|(i, c)| Some((u8::try_from(i).ok()?, (*c)?)))
                .collect()
        });
        PaletteState {
            entries,
            fg: self.fg,
            bg: self.bg,
            cursor: self.cursor,
        }
    }

    /// Take `state`, keeping the colour name check.
    pub(crate) fn restore(&mut self, state: &PaletteState) {
        self.entries = (!state.entries.is_empty()).then(|| {
            let mut e = Box::new([None; 256]);
            for &(i, c) in &state.entries {
                e[usize::from(i)] = Some(c);
            }
            e
        });
        self.fg = state.fg;
        self.bg = state.bg;
        self.cursor = state.cursor;
    }

    pub(crate) fn set_name_check(&mut self, names: NameCheck) {
        self.names = names;
    }

    /// RIS and `OSC 104` with no argument: `colour_palette_clear`.
    pub(crate) fn clear(&mut self) {
        self.entries = None;
        self.fg = None;
        self.bg = None;
    }

    /// An OSC ended `end`: true when tmux calls `screen_write_fullredraw`.
    /// What it asks of the server goes on `out`.
    pub(crate) fn osc(&mut self, payload: &[u8], end: OscEnd, out: &mut Vec<Reply>) -> bool {
        let Some((n, p)) = osc_number(payload) else {
            return false;
        };
        match n {
            4 => self.osc_4(p, end, out),
            10 | 11 if p == b"?" => {
                out.push(if n == 10 {
                    Reply::Foreground(end)
                } else {
                    Reply::Background(end)
                });
                false
            }
            10 | 11 => {
                let Some(c) = self.parse_x11(p) else {
                    return false;
                };
                if n == 10 {
                    self.fg = Some(c);
                } else {
                    self.bg = Some(c);
                }
                true
            }
            12 => {
                if p == b"?" {
                    out.push(Reply::Cursor {
                        colour: self.cursor,
                        end,
                    });
                } else if let Some(c) = self.parse_x11(p) {
                    self.cursor = Some(c);
                }
                false
            }
            52 => {
                osc_52(p, end, out);
                false
            }
            104 => self.osc_104(p),
            110 | 111 if p.is_empty() => {
                if n == 110 {
                    self.fg = None;
                } else {
                    self.bg = None;
                }
                true
            }
            112 if p.is_empty() => {
                self.cursor = None;
                false
            }
            _ => false,
        }
    }

    fn osc_4(&mut self, p: &[u8], end: OscEnd, out: &mut Vec<Reply>) -> bool {
        let mut redraw = false;
        let mut s = Some(p);
        while let Some(cur) = s.filter(|c| !c.is_empty()) {
            let (idx, next) = strtol(cur);
            let Some(rest) = next.strip_prefix(b";") else {
                break;
            };
            let Ok(idx) = u8::try_from(idx) else {
                break;
            };
            let (arg, after) = match rest.iter().position(|b| *b == b';') {
                Some(i) => (&rest[..i], Some(&rest[i + 1..])),
                None => (rest, None),
            };
            s = after;
            if arg == b"?" {
                let set = self.entries.as_ref().and_then(|e| e[usize::from(idx)]);
                out.push(match set {
                    Some(c) => Reply::Bytes(colour_reply(4, Some(idx), c, end)),
                    None => Reply::Palette { index: idx, end },
                });
            } else if let Some(c) = self.parse_x11(arg) {
                // colour_palette_set allocates the palette.
                self.entries.get_or_insert_with(|| Box::new([None; 256]))[usize::from(idx)] =
                    Some(c);
                redraw = true;
            }
        }
        redraw
    }

    fn osc_104(&mut self, p: &[u8]) -> bool {
        if p.is_empty() {
            self.clear();
            return true;
        }
        let mut redraw = false;
        let mut s = p;
        while !s.is_empty() {
            let (idx, next) = strtol(s);
            if !matches!(next.first(), None | Some(b';')) {
                break;
            }
            let Ok(idx) = u8::try_from(idx) else {
                break;
            };
            // colour_palette_set(p, idx, -1) does nothing until a colour was set.
            if let Some(e) = self.entries.as_mut() {
                e[usize::from(idx)] = None;
                redraw = true;
            }
            s = next.strip_prefix(b";").unwrap_or(next);
        }
        redraw
    }

    fn parse_x11(&self, p: &[u8]) -> Option<Rgb> {
        parse_x11(p, self.names)
    }
}

/// `colour_parseX11` (colour.c:1176-1213), with `names` for
/// `colour_byname`.
#[must_use]
pub fn parse_x11(p: &[u8], names: NameCheck) -> Option<Rgb> {
    let len = p.len();
    let byte = |v: u32| v.to_le_bytes()[0];
    let short = (len == 12)
        .then(|| hex_triple(p, b"rgb:", b"/", 2))
        .flatten()
        .or_else(|| (len == 7).then(|| hex_triple(p, b"#", b"", 2)).flatten())
        .or_else(|| int_triple(p));
    if let Some([r, g, b]) = short {
        return Some((byte(r), byte(g), byte(b)));
    }
    let long = (len == 18)
        .then(|| hex_triple(p, b"rgb:", b"/", 4))
        .flatten()
        .or_else(|| (len == 13).then(|| hex_triple(p, b"#", b"", 4)).flatten());
    if let Some([r, g, b]) = long {
        return Some((byte(r >> 8), byte(g >> 8), byte(b >> 8)));
    }
    if let Some(c) = cmyk(p) {
        return Some(c);
    }
    std::str::from_utf8(p)
        .ok()
        .and_then(|t| names(t.trim_matches(' ')))
}

/// `input_osc_52_parse` (input.c:3371-3413), before its `set-clipboard`
/// check, which is the server's: the clipboard names before the `;` that
/// tmux allows, once each, and `?` or the base64 text after it.
fn osc_52(p: &[u8], end: OscEnd, out: &mut Vec<Reply>) {
    const ALLOW: &[u8] = b"cpqs01234567";
    let Some(at) = p.iter().position(|&b| b == b';') else {
        return;
    };
    let data = &p[at + 1..];
    if data.is_empty() {
        return;
    }
    let mut clip = Vec::new();
    for &b in &p[..at] {
        if ALLOW.contains(&b) && !clip.contains(&b) {
            clip.push(b);
        }
    }
    out.push(if data == b"?" {
        Reply::Clipboard { clip, end }
    } else {
        Reply::SetClipboard {
            clip,
            data: data.to_vec(),
        }
    });
}

/// C `isspace`.
fn c_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `strtol(s, &end, 10)`: the value (saturated) and the rest. Without digits
/// the value is 0 and the rest is all of `s`.
fn strtol(s: &[u8]) -> (i64, &[u8]) {
    let at = s.iter().take_while(|b| c_space(**b)).count();
    let mut i = at;
    let neg = s.get(i) == Some(&b'-');
    if matches!(s.get(i), Some(b'+' | b'-')) {
        i += 1;
    }
    let digits = s[i..].iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return (0, s);
    }
    let v = s[i..i + digits].iter().fold(0i64, |a, b| {
        a.saturating_mul(10).saturating_add(i64::from(b - b'0'))
    });
    (if neg { -v } else { v }, &s[i + digits..])
}

/// `sscanf`'s `%<width>x` conversion: the value and the rest after it, or
/// `None` when no digit converted. White space before it is skipped and
/// does not count toward the width; a sign and `0x` do.
fn scan_hex(s: &[u8], width: usize) -> Option<(u32, &[u8])> {
    let start = s.iter().take_while(|b| c_space(**b)).count();
    let mut i = start;
    let neg = s.get(i) == Some(&b'-');
    if matches!(s.get(i), Some(b'+' | b'-')) && i + 1 - start <= width {
        i += 1;
    }
    if s.get(i) == Some(&b'0')
        && matches!(s.get(i + 1), Some(b'x' | b'X'))
        && i + 2 - start < width
        && s.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
    {
        i += 2;
    }
    let room = width.saturating_sub(i - start);
    let digits = s[i..]
        .iter()
        .take(room)
        .take_while(|b| b.is_ascii_hexdigit())
        .count();
    if digits == 0 {
        return None;
    }
    let v = s[i..i + digits].iter().fold(0u32, |a, &b| {
        let d = char::from(b).to_digit(16).unwrap_or(0);
        a.wrapping_mul(16).wrapping_add(d)
    });
    Some((if neg { v.wrapping_neg() } else { v }, &s[i + digits..]))
}

/// `sscanf(p, "<prefix>%Nx<sep>%Nx<sep>%Nx") == 3`: the three values.
fn hex_triple(p: &[u8], prefix: &[u8], sep: &[u8], width: usize) -> Option<[u32; 3]> {
    let mut rest = p.strip_prefix(prefix)?;
    let mut out = [0; 3];
    for (i, slot) in out.iter_mut().enumerate() {
        if i > 0 {
            rest = rest.strip_prefix(sep)?;
        }
        let (v, r) = scan_hex(rest, width)?;
        *slot = v;
        rest = r;
    }
    Some(out)
}

/// `sscanf(p, "%d,%d,%d") == 3` into `u_int`s: the three values.
fn int_triple(p: &[u8]) -> Option<[u32; 3]> {
    let mut rest = p;
    let mut out = [0; 3];
    for (i, slot) in out.iter_mut().enumerate() {
        if i > 0 {
            rest = rest.strip_prefix(b",")?;
        }
        let (v, r) = strtol(rest);
        if r.len() == rest.len() {
            return None;
        }
        // `%d` into a `u_int`: the int's bits.
        *slot = i32::try_from(v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)))
            .unwrap_or(0)
            .cast_unsigned();
        rest = r;
    }
    Some(out)
}

/// `sscanf` `%lf`: the longest prefix that is a decimal float, after white
/// space.
fn scan_f64(s: &[u8]) -> Option<(f64, &[u8])> {
    let at = s.iter().take_while(|b| c_space(**b)).count();
    let body = &s[at..];
    let ok = |n: usize| {
        body[..n]
            .iter()
            .all(|b| b.is_ascii_digit() || b".+-eE".contains(b))
            && std::str::from_utf8(&body[..n]).is_ok_and(|t| t.parse::<f64>().is_ok())
    };
    let n = (1..=body.len()).rev().find(|&n| ok(n))?;
    let v = std::str::from_utf8(&body[..n]).ok()?.parse().ok()?;
    Some((v, &body[n..]))
}

/// The `cmyk:` and `cmy:` forms (colour.c:1192-1199): every component in
/// 0..=1, each channel `(1 - c) * (1 - k) * 255` truncated.
fn cmyk(text: &[u8]) -> Option<Rgb> {
    let read = |prefix: &[u8], n: usize| -> Option<[f64; 4]> {
        let mut rest = text.strip_prefix(prefix)?;
        let mut out = [0.0; 4];
        for (i, slot) in out.iter_mut().take(n).enumerate() {
            if i > 0 {
                rest = rest.strip_prefix(b"/")?;
            }
            let (v, r) = scan_f64(rest)?;
            *slot = v;
            rest = r;
        }
        Some(out)
    };
    let [cyan, magenta, yellow, black] = read(b"cmyk:", 4)
        .or_else(|| read(b"cmy:", 3))
        .filter(|v| v.iter().all(|x| (0.0..=1.0).contains(x)))?;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "0..=255 by the range check; C truncates the double"
    )]
    let channel = |v: f64| ((1.0 - v) * (1.0 - black) * 255.0) as u8;
    Some((channel(cyan), channel(magenta), channel(yellow)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        let mut p = Palette::default();
        p.set_name_check(|n| n.eq_ignore_ascii_case("red").then_some((255, 0, 0)));
        p
    }

    fn osc(p: &mut Palette, payload: &[u8]) -> bool {
        p.osc(payload, OscEnd::Bel, &mut Vec::new())
    }

    fn replies(p: &mut Palette, payload: &[u8]) -> Vec<Reply> {
        let mut out = Vec::new();
        p.osc(payload, OscEnd::Bel, &mut out);
        out
    }

    #[test]
    fn osc_4_redraws_only_for_a_parsed_colour() {
        let mut p = palette();
        assert!(osc(&mut p, b"4;1;#ff0000"));
        assert!(osc(&mut p, b"4;1;rgb:ff/00/00"));
        assert!(osc(&mut p, b"4;1;rgb:ffff/0000/0000"));
        assert!(osc(&mut p, b"4;1;1,2,3"));
        assert!(osc(&mut p, b"4;1;cmyk:0/0/0/0"));
        assert!(osc(&mut p, b"4;1; red "));
        assert!(osc(&mut p, b"4;1;?;2;red"));
        assert!(
            osc(&mut p, b"4;;red"),
            "no digits is entry 0 when `;` follows"
        );
        for bad in [
            &b"4;1;zzz"[..],
            b"4;1;?",
            b"4;256;red",
            b"4;x;red",
            b"4;1",
            b"4;1;cmy:2/0/0",
            b"4;1;#ff00",
            b"4",
        ] {
            assert!(!osc(&mut p, bad), "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn osc_104_redraws_when_an_entry_was_set() {
        let mut p = palette();
        assert!(!osc(&mut p, b"104;1"), "nothing set yet");
        assert!(osc(&mut p, b"104"), "clear-all always redraws");
        assert!(osc(&mut p, b"104;"));
        assert!(!osc(&mut p, b"104;1;2"));
        osc(&mut p, b"4;1;red");
        assert!(osc(&mut p, b"104;1"));
        assert!(osc(&mut p, b"104;1"), "the palette stays allocated");
        assert!(
            osc(&mut p, b"104;3;x"),
            "an entry before a bad one still counts"
        );
        assert!(!osc(&mut p, b"104;x"));
        assert!(osc(&mut p, b"104"));
        assert!(!osc(&mut p, b"104;1"));
    }

    #[test]
    fn default_colour_oscs() {
        let mut p = palette();
        for set in [&b"10;red"[..], b"11;#000000", b"10;1,2,3"] {
            assert!(osc(&mut p, set));
        }
        for no in [
            &b"10;?"[..],
            b"11;?",
            b"10;nope",
            b"12;red",
            b"112",
            b"110;x",
        ] {
            assert!(!osc(&mut p, no));
        }
        assert!(osc(&mut p, b"110"));
        assert!(osc(&mut p, b"111"));
        assert!(osc(&mut p, b"111;"));
    }

    /// `colour_parseX11` (colour.c:1176-1213), the forms osc-11colours.sh
    /// sets.
    #[test]
    fn x11_forms_parse_to_tmux_values() {
        let p = palette();
        for (form, want) in [
            (&b"rgb:ff/ff/ff"[..], (0xff, 0xff, 0xff)),
            (b"rgb:10/4E/8B", (0x10, 0x4e, 0x8b)),
            (b"rgb:1000/4E00/8B00", (0x10, 0x4e, 0x8b)),
            (b"#104E8B", (0x10, 0x4e, 0x8b)),
            (b"#10004E008B00", (0x10, 0x4e, 0x8b)),
            (b"16,78,139", (0x10, 0x4e, 0x8b)),
            (b"cmy:0.9373/0.6941/0.4549", (0x0f, 0x4e, 0x8b)),
            (b"cmyk:0.88/0.44/0.00/0.45", (0x10, 0x4e, 0x8c)),
            (b"  red  ", (0xff, 0, 0)),
        ] {
            assert_eq!(
                p.parse_x11(form),
                Some(want),
                "{}",
                String::from_utf8_lossy(form)
            );
        }
    }

    #[test]
    fn queries_become_replies() {
        let mut p = palette();
        assert_eq!(replies(&mut p, b"10;?"), [Reply::Foreground(OscEnd::Bel)]);
        assert_eq!(replies(&mut p, b"11;?"), [Reply::Background(OscEnd::Bel)]);
        assert_eq!(
            replies(&mut p, b"4;1;?"),
            [Reply::Palette {
                index: 1,
                end: OscEnd::Bel
            }]
        );
        osc(&mut p, b"4;1;red");
        assert_eq!(
            replies(&mut p, b"4;1;?"),
            [Reply::Bytes(b"\x1b]4;1;rgb:ffff/0000/0000\x07".to_vec())]
        );
        osc(&mut p, b"104;1");
        assert!(matches!(
            replies(&mut p, b"4;1;?")[..],
            [Reply::Palette { .. }]
        ));
        osc(&mut p, b"12;#00ff00");
        assert_eq!(
            replies(&mut p, b"12;?"),
            [Reply::Cursor {
                colour: Some((0, 0xff, 0)),
                end: OscEnd::Bel
            }]
        );
        assert_eq!(
            replies(&mut p, b"52;cc;?"),
            [Reply::Clipboard {
                clip: b"c".to_vec(),
                end: OscEnd::Bel
            }]
        );
        assert_eq!(
            replies(&mut p, b"52;xp;aGk="),
            [Reply::SetClipboard {
                clip: b"p".to_vec(),
                data: b"aGk=".to_vec()
            }]
        );
        assert_eq!(replies(&mut p, b"52;c;"), []);
        assert_eq!(replies(&mut p, b"52"), []);
    }
}
