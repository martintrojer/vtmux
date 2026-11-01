//! The palette OSCs that make tmux redraw the pane (input.c `input_osc_4`,
//! `_10`, `_11`, `_104`, `_110`, `_111`).
//!
//! Each calls `screen_write_fullredraw` when it changes the pane's palette,
//! which sets `PANE_REDRAW`; a passthrough payload later in the same input
//! is then dropped under `allow-passthrough on` (screen-write.c:150). Only
//! whether a redraw happens is modelled here: Ghostty keeps the colours.

/// `colour_byname` stand-in: the X11 name table lives in `vtmux-core`.
pub(crate) type NameCheck = fn(&str) -> bool;

/// tmux's per-pane `colour_palette`: whether `palette->palette` is allocated
/// (`colour_palette_set` returns 0 for a reset while it is not).
#[derive(Debug)]
pub(crate) struct Palette {
    allocated: bool,
    names: NameCheck,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            allocated: false,
            names: |_| false,
        }
    }
}

/// Whether an OSC with this payload starts like one of the palette OSCs
/// (the full payload is worth keeping).
pub(crate) fn is_palette_osc(prefix: &[u8]) -> bool {
    matches!(osc_number(prefix), Some((4 | 10 | 11 | 104 | 110 | 111, _)))
}

/// `input_exit_osc`'s split: the number (a `u_int`, wrapping) and the rest
/// after its `;`.
fn osc_number(p: &[u8]) -> Option<(u32, &[u8])> {
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

impl Palette {
    pub(crate) fn set_name_check(&mut self, names: NameCheck) {
        self.names = names;
    }

    /// RIS and `OSC 104` with no argument: `colour_palette_clear`.
    pub(crate) fn clear(&mut self) {
        self.allocated = false;
    }

    /// An OSC ended: true when tmux calls `screen_write_fullredraw`.
    pub(crate) fn osc(&mut self, payload: &[u8]) -> bool {
        let Some((n, p)) = osc_number(payload) else {
            return false;
        };
        match n {
            4 => self.osc_4(p),
            10 | 11 => p != b"?" && self.parses(p),
            104 => self.osc_104(p),
            110 | 111 => p.is_empty(),
            _ => false,
        }
    }

    fn osc_4(&mut self, p: &[u8]) -> bool {
        let mut redraw = false;
        let mut s = Some(p);
        while let Some(cur) = s.filter(|c| !c.is_empty()) {
            let (idx, next) = strtol(cur);
            let Some(rest) = next.strip_prefix(b";") else {
                break;
            };
            if !(0..256).contains(&idx) {
                break;
            }
            let (arg, after) = match rest.iter().position(|b| *b == b';') {
                Some(i) => (&rest[..i], Some(&rest[i + 1..])),
                None => (rest, None),
            };
            s = after;
            if arg != b"?" && self.parses(arg) {
                // colour_palette_set allocates the palette.
                self.allocated = true;
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
            if !matches!(next.first(), None | Some(b';')) || !(0..256).contains(&idx) {
                break;
            }
            // colour_palette_set(p, idx, -1) does nothing until a colour was set.
            redraw |= self.allocated;
            s = next.strip_prefix(b";").unwrap_or(next);
        }
        redraw
    }

    /// `colour_parseX11` (colour.c:1176): does it find a colour?
    fn parses(&self, p: &[u8]) -> bool {
        let len = p.len();
        if (len == 12 && hex_triple(p, b"rgb:", b"/", 2))
            || (len == 7 && hex_triple(p, b"#", b"", 2))
            || int_triple(p)
            || (len == 18 && hex_triple(p, b"rgb:", b"/", 4))
            || (len == 13 && hex_triple(p, b"#", b"", 4))
            || cmyk(p)
        {
            return true;
        }
        std::str::from_utf8(p).is_ok_and(|t| (self.names)(t.trim_matches(' ')))
    }
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

/// `sscanf`'s `%<width>x` conversion: the rest after it, or `None` when no
/// digit converted. White space before it is skipped and does not count
/// toward the width; a sign and `0x` do.
fn scan_hex(s: &[u8], width: usize) -> Option<&[u8]> {
    let start = s.iter().take_while(|b| c_space(**b)).count();
    let mut i = start;
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
    (digits != 0).then(|| &s[i + digits..])
}

/// `sscanf(p, "<prefix>%Nx<sep>%Nx<sep>%Nx") == 3`.
fn hex_triple(p: &[u8], prefix: &[u8], sep: &[u8], width: usize) -> bool {
    let Some(mut rest) = p.strip_prefix(prefix) else {
        return false;
    };
    for i in 0..3 {
        if i > 0 {
            let Some(r) = rest.strip_prefix(sep) else {
                return false;
            };
            rest = r;
        }
        let Some(r) = scan_hex(rest, width) else {
            return false;
        };
        rest = r;
    }
    true
}

/// `sscanf(p, "%d,%d,%d") == 3`.
fn int_triple(p: &[u8]) -> bool {
    let mut rest = p;
    for i in 0..3 {
        if i > 0 {
            let Some(r) = rest.strip_prefix(b",") else {
                return false;
            };
            rest = r;
        }
        let (_, r) = strtol(rest);
        if r.len() == rest.len() {
            return false;
        }
        rest = r;
    }
    true
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

/// The `cmyk:` and `cmy:` forms (colour.c:1192-1195): every component in 0..=1.
fn cmyk(p: &[u8]) -> bool {
    let read = |prefix: &[u8], n: usize| -> Option<[f64; 4]> {
        let mut rest = p.strip_prefix(prefix)?;
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
    read(b"cmyk:", 4)
        .or_else(|| read(b"cmy:", 3))
        .is_some_and(|v| v.iter().all(|x| (0.0..=1.0).contains(x)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        let mut p = Palette::default();
        p.set_name_check(|n| n.eq_ignore_ascii_case("red"));
        p
    }

    #[test]
    fn osc_4_redraws_only_for_a_parsed_colour() {
        let mut p = palette();
        assert!(p.osc(b"4;1;#ff0000"));
        assert!(p.osc(b"4;1;rgb:ff/00/00"));
        assert!(p.osc(b"4;1;rgb:ffff/0000/0000"));
        assert!(p.osc(b"4;1;1,2,3"));
        assert!(p.osc(b"4;1;cmyk:0/0/0/0"));
        assert!(p.osc(b"4;1; red "));
        assert!(p.osc(b"4;1;?;2;red"));
        assert!(p.osc(b"4;;red"), "no digits is entry 0 when `;` follows");
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
            assert!(!p.osc(bad), "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn osc_104_redraws_when_an_entry_was_set() {
        let mut p = palette();
        assert!(!p.osc(b"104;1"), "nothing set yet");
        assert!(p.osc(b"104"), "clear-all always redraws");
        assert!(p.osc(b"104;"));
        assert!(!p.osc(b"104;1;2"));
        p.osc(b"4;1;red");
        assert!(p.osc(b"104;1"));
        assert!(p.osc(b"104;1"), "the palette stays allocated");
        assert!(p.osc(b"104;3;x"), "an entry before a bad one still counts");
        assert!(!p.osc(b"104;x"));
        assert!(p.osc(b"104"));
        assert!(!p.osc(b"104;1"));
    }

    #[test]
    fn default_colour_oscs() {
        let mut p = palette();
        for set in [&b"10;red"[..], b"11;#000000", b"10;1,2,3"] {
            assert!(p.osc(set));
        }
        for no in [
            &b"10;?"[..],
            b"11;?",
            b"10;nope",
            b"12;red",
            b"112",
            b"110;x",
        ] {
            assert!(!p.osc(no));
        }
        assert!(p.osc(b"110"));
        assert!(p.osc(b"111"));
        assert!(p.osc(b"111;"));
    }
}
