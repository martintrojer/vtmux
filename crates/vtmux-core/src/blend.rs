//! The value of `vtmux-pane-active-blend` / `vtmux-pane-inactive-blend`
//! (D26, `docs/specs/2026-11-04-pane-blend.md`): `fg=<colour>[/<pct>]` and
//! `bg=<colour>[/<pct>]`, either optional, in any order, separated like a
//! style (`,` or white space). `<colour>` is anything a style takes, or
//! `fg` / `bg` for the outer terminal's default colours; `<pct>` (0-100,
//! default 100) is how far each cell's colour moves toward it.
//!
//! The empty value is off. The per-cell arithmetic is [`mix`].

use crate::colour::Colour;
use crate::style::InvalidStyle;

/// What a blend moves a colour toward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Toward {
    /// `bg`: the outer terminal's default background (its OSC 11 reply).
    TerminalBg,
    /// `fg`: the outer terminal's default foreground (its OSC 10 reply).
    TerminalFg,
    /// A style colour. `default` and `terminal` mean the terminal's own
    /// default for the part it is given to.
    Colour(Colour),
}

/// One part of a blend: the target and how far to go, 0-100.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    pub toward: Toward,
    pub pct: u8,
}

/// A parsed blend option; both parts `None` never comes out of [`parse`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Blend {
    /// Where glyph colours (and underline colours) go.
    pub fg: Option<Part>,
    /// Where background colours go.
    pub bg: Option<Part>,
}

/// The option value as a blend: `Ok(None)` for the empty value (off);
/// anything else that is not `fg=`/`bg=` parts is tmux's style error.
pub fn parse(value: &str) -> Result<Option<Blend>, InvalidStyle> {
    let bad = || InvalidStyle(value.to_owned());
    let mut blend = Blend::default();
    for token in value
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|t| !t.is_empty())
    {
        let (key, rest) = token.split_once('=').ok_or_else(bad)?;
        let part = parse_part(rest).ok_or_else(bad)?;
        match key.to_ascii_lowercase().as_str() {
            "fg" => blend.fg = Some(part),
            "bg" => blend.bg = Some(part),
            _ => return Err(bad()),
        }
    }
    Ok((blend != Blend::default()).then_some(blend))
}

/// `<colour>[/<pct>]`.
fn parse_part(s: &str) -> Option<Part> {
    let (colour, pct) = match s.split_once('/') {
        Some((c, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
            (c, p.parse::<u8>().ok().filter(|&p| p <= 100)?)
        }
        Some(_) => return None,
        None => (s, 100),
    };
    let toward = if colour.eq_ignore_ascii_case("bg") {
        Toward::TerminalBg
    } else if colour.eq_ignore_ascii_case("fg") {
        Toward::TerminalFg
    } else {
        Toward::Colour(Colour::parse(colour)?)
    };
    Some(Part { toward, pct })
}

/// `from` moved `pct` percent of the way to `to`, per sRGB channel, rounded.
#[must_use]
pub fn mix(from: (u8, u8, u8), to: (u8, u8, u8), pct: u8) -> (u8, u8, u8) {
    let p = u32::from(pct.min(100));
    let one = |a: u8, b: u8| {
        let v = (u32::from(a) * (100 - p) + u32::from(b) * p + 50) / 100;
        u8::try_from(v).unwrap_or(u8::MAX)
    };
    (one(from.0, to.0), one(from.1, to.1), one(from.2, to.2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(toward: Toward, pct: u8) -> Part {
        Part { toward, pct }
    }

    #[test]
    fn both_parts_either_part_any_order() {
        assert_eq!(
            parse("fg=bg/25,bg=bg/25"),
            Ok(Some(Blend {
                fg: Some(part(Toward::TerminalBg, 25)),
                bg: Some(part(Toward::TerminalBg, 25)),
            }))
        );
        assert_eq!(
            parse("bg=#181825 fg=FG/35"),
            Ok(Some(Blend {
                fg: Some(part(Toward::TerminalFg, 35)),
                bg: Some(part(Toward::Colour(Colour::Rgb(0x18, 0x18, 0x25)), 100)),
            }))
        );
        assert_eq!(
            parse("bg=colour235/0"),
            Ok(Some(Blend {
                fg: None,
                bg: Some(part(Toward::Colour(Colour::Palette(235)), 0)),
            }))
        );
        assert_eq!(
            parse("fg=red/100"),
            Ok(Some(Blend {
                fg: Some(part(Toward::Colour(Colour::Ansi(1)), 100)),
                bg: None,
            }))
        );
        // The last of a part wins, as in a style.
        assert_eq!(
            parse("bg=red,bg=blue").unwrap().unwrap().bg,
            Some(part(Toward::Colour(Colour::Ansi(4)), 100))
        );
    }

    #[test]
    fn empty_is_off() {
        assert_eq!(parse(""), Ok(None));
        assert_eq!(parse(" , "), Ok(None));
    }

    #[test]
    fn bad_values_are_style_errors() {
        for bad in [
            "fg=nope",
            "fg=bg/101",
            "fg=bg/-1",
            "fg=bg/",
            "fg=bg/x",
            "fg=bg/2/3",
            "bold",
            "us=red",
            "fg",
            "fg=",
            "dim=bg/25",
        ] {
            let e = parse(bad).expect_err(bad);
            assert_eq!(e.to_string(), format!("invalid style: {bad}"));
        }
    }

    #[test]
    fn mix_moves_toward_the_target() {
        let (a, b) = ((200, 100, 0), (0, 0, 255));
        assert_eq!(mix(a, b, 0), a);
        assert_eq!(mix(a, b, 100), b);
        assert_eq!(mix(a, b, 25), (150, 75, 64));
        assert_eq!(
            mix((255, 255, 255), (0x1e, 0x1e, 0x2e), 25),
            (199, 199, 203)
        );
    }
}
