//! What a client terminal is sent for a colour it cannot show: tmux 3.8's
//! `tty_check_fg` / `tty_check_bg` (tty.c:2842-2960), `colour_find_rgb` and
//! `colour_256to16` (colour.c:137, 540).
//!
//! The inputs and outputs are [`Colour`]s with tmux's flag bits spelled out:
//! `Ansi` is SGR 30-37/40-47, `Bright` the aixterm 90-97/100-107, `Palette`
//! a `38;5;N` colour. Pure; pinned by `tty_cases.tsv`, which
//! `tests/style_cases.sh tty_colour` regenerates from the reference client's
//! output.

use super::Colour;

/// The part of a client's terminal that decides the downgrade.
#[derive(Clone, Copy, Debug)]
pub struct Depth {
    /// `TERM_RGBCOLOURS`.
    pub rgb: bool,
    /// `Term::palette_size`: 256 under the 256/RGB feature, else terminfo's
    /// `colors`.
    pub colours: u32,
}

/// `colour_find_rgb`: the closest entry of xterm's 6x6x6 cube or grey ramp.
#[must_use]
pub fn find_rgb(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [i32; 6] = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
    let cube = |v: u8| match v {
        0..=47 => 0,
        48..=113 => 1,
        _ => (usize::from(v) - 35) / 40,
    };
    let (qr, qg, qb) = (cube(r), cube(g), cube(b));
    let (cr, cg, cb) = (LEVELS[qr], LEVELS[qg], LEVELS[qb]);
    let (ri, gi, bi) = (i32::from(r), i32::from(g), i32::from(b));
    let index = u8::try_from(16 + 36 * qr + 6 * qg + qb).unwrap_or(16);
    if (cr, cg, cb) == (ri, gi, bi) {
        return index;
    }
    let grey_avg = (ri + gi + bi) / 3;
    let grey_idx = if grey_avg > 238 {
        23
    } else {
        (grey_avg - 3) / 10
    };
    let grey = 8 + 10 * grey_idx;
    let dist = |x: i32, y: i32, z: i32| (x - ri).pow(2) + (y - gi).pow(2) + (z - bi).pow(2);
    if dist(grey, grey, grey) < dist(cr, cg, cb) {
        232 + u8::try_from(grey_idx).unwrap_or(0)
    } else {
        index
    }
}

/// `colour_256to16`.
#[must_use]
pub fn to_16(n: u8) -> u8 {
    #[rustfmt::skip]
    const TABLE: [u8; 256] = [
         0,  1,  2,  3,  4,  5,  6,  7,  8,  9, 10, 11, 12, 13, 14, 15,
         0,  4,  4,  4, 12, 12,  2,  6,  4,  4, 12, 12,  2,  2,  6,  4,
        12, 12,  2,  2,  2,  6, 12, 12, 10, 10, 10, 10, 14, 12, 10, 10,
        10, 10, 10, 14,  1,  5,  4,  4, 12, 12,  3,  8,  4,  4, 12, 12,
         2,  2,  6,  4, 12, 12,  2,  2,  2,  6, 12, 12, 10, 10, 10, 10,
        14, 12, 10, 10, 10, 10, 10, 14,  1,  1,  5,  4, 12, 12,  1,  1,
         5,  4, 12, 12,  3,  3,  8,  4, 12, 12,  2,  2,  2,  6, 12, 12,
        10, 10, 10, 10, 14, 12, 10, 10, 10, 10, 10, 14,  1,  1,  1,  5,
        12, 12,  1,  1,  1,  5, 12, 12,  1,  1,  1,  5, 12, 12,  3,  3,
         3,  7, 12, 12, 10, 10, 10, 10, 14, 12, 10, 10, 10, 10, 10, 14,
         9,  9,  9,  9, 13, 12,  9,  9,  9,  9, 13, 12,  9,  9,  9,  9,
        13, 12,  9,  9,  9,  9, 13, 12, 11, 11, 11, 11,  7, 12, 10, 10,
        10, 10, 10, 14,  9,  9,  9,  9,  9, 13,  9,  9,  9,  9,  9, 13,
         9,  9,  9,  9,  9, 13,  9,  9,  9,  9,  9, 13,  9,  9,  9,  9,
         9, 13, 11, 11, 11, 11, 11, 15,  0,  0,  0,  0,  0,  0,  8,  8,
         8,  8,  8,  8,  7,  7,  7,  7,  7,  7, 15, 15, 15, 15, 15, 15,
    ];
    TABLE[usize::from(n)]
}

/// The first half of both checks: an RGB colour on a terminal without RGB
/// becomes a 256-colour one.
fn flatten(colour: Colour, depth: Depth) -> Colour {
    match colour {
        Colour::Rgb(r, g, b) if !depth.rgb => Colour::Palette(find_rgb(r, g, b)),
        other => other,
    }
}

/// The 256-colour case of the checks, for `colours < 256`: `colour_256to16`,
/// and whether the result is sent as aixterm (it is bright and the terminal
/// has 16 colours).
fn palette_below_256(n: u8, d: Depth) -> (u8, bool) {
    let c = to_16(n);
    (c, c >= 8 && d.colours >= 16)
}

/// `tty_check_fg`: the foreground to send, and whether the bright attribute
/// (SGR 1) is added. `bg` is the cell's background as the pane stored it.
#[must_use]
pub fn check_fg(fg: Colour, bg: Colour, d: Depth) -> (Colour, bool) {
    match flatten(fg, d) {
        Colour::Palette(n) if d.colours < 256 => {
            let (c, aixterm) = palette_below_256(n, d);
            if c < 8 {
                (Colour::Ansi(c), false)
            } else if aixterm {
                (Colour::Bright(c & 7), false)
            } else {
                // Black on black or white on white is no use: change the
                // foreground.
                let c = c & 7;
                let c = match (c, bg) {
                    (0, Colour::Ansi(0)) => 7,
                    (7, Colour::Ansi(7)) => 0,
                    _ => c,
                };
                (Colour::Ansi(c), false)
            }
        }
        Colour::Bright(n) if d.colours < 16 => (Colour::Ansi(n), true),
        other => (other, false),
    }
}

/// `tty_check_bg`: bold backgrounds do not exist portably, so the bright
/// forms drop to the plain colour on 8-colour terminals.
#[must_use]
pub fn check_bg(bg: Colour, d: Depth) -> Colour {
    match flatten(bg, d) {
        Colour::Palette(n) if d.colours < 256 => {
            let (c, aixterm) = palette_below_256(n, d);
            if c < 8 {
                Colour::Ansi(c)
            } else if aixterm {
                Colour::Bright(c & 7)
            } else {
                Colour::Ansi(c & 7)
            }
        }
        Colour::Bright(n) if d.colours < 16 => Colour::Ansi(n),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TERM colours SGR-params... -> state` (see `tests/style_cases.sh`).
    const CASES: &str = include_str!("tty_cases.tsv");

    fn parse(param: &str) -> Colour {
        let nums: Vec<u8> = param.split(';').map(|s| s.parse().unwrap()).collect();
        match nums[..] {
            [38 | 48, 5, n] => Colour::Palette(n),
            [38 | 48, 2, r, g, b] => Colour::Rgb(r, g, b),
            [n @ (30..=37 | 40..=47)] => Colour::Ansi(n % 10),
            [n @ 90..=97] => Colour::Bright(n - 90),
            [n @ 100..=107] => Colour::Bright(n - 100),
            _ => panic!("{param}"),
        }
    }

    fn sgr(c: Colour, bg: bool) -> String {
        let base = if bg { 40 } else { 30 };
        match c {
            Colour::Default => "-".into(),
            Colour::Ansi(n) => (base + u32::from(n)).to_string(),
            Colour::Bright(n) => (base + 60 + u32::from(n)).to_string(),
            Colour::Palette(n) => format!("{};5;{n}", base + 8),
            Colour::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", base + 8),
            _ => unreachable!(),
        }
    }

    #[test]
    fn matches_the_reference_client() {
        let mut rows = 0;
        for line in CASES.lines().filter(|l| !l.starts_with('#')) {
            let [colours, input, want] = line.split('\t').collect::<Vec<_>>()[..] else {
                panic!("{line}");
            };
            let d = Depth {
                rgb: false,
                colours: colours.parse().unwrap(),
            };
            let (mut fg, mut bg) = (Colour::Default, Colour::Default);
            for p in input.split(' ') {
                let c = parse(p);
                if p.starts_with(['3', '9']) || p.starts_with("38") {
                    fg = c;
                } else {
                    bg = c;
                }
            }
            let (f, bold) = check_fg(fg, bg, d);
            let got = format!(
                "fg={} bg={}{}",
                sgr(f, false),
                sgr(check_bg(bg, d), true),
                if bold { " bold" } else { "" }
            );
            assert_eq!(got, want, "{colours} colours, {input}");
            rows += 1;
        }
        assert!(rows > 1000, "{rows}");
    }
}
