//! Host-independent glibc character properties, generated in the reference locale.
use super::ctype_tables as tables;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    Alpha,
    Alnum,
    Upper,
    Lower,
    Digit,
    Xdigit,
    Space,
    Blank,
    Cntrl,
    Graph,
    Print,
    Punct,
}

impl Class {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "alpha" => Self::Alpha,
            "alnum" => Self::Alnum,
            "upper" => Self::Upper,
            "lower" => Self::Lower,
            "digit" => Self::Digit,
            "xdigit" => Self::Xdigit,
            "space" => Self::Space,
            "blank" => Self::Blank,
            "cntrl" => Self::Cntrl,
            "graph" => Self::Graph,
            "print" => Self::Print,
            "punct" => Self::Punct,
            _ => return None,
        })
    }

    pub(crate) fn contains(self, c: char) -> bool {
        if c.is_ascii() {
            return match self {
                Self::Alpha => c.is_ascii_alphabetic(),
                Self::Alnum => c.is_ascii_alphanumeric(),
                Self::Upper => c.is_ascii_uppercase(),
                Self::Lower => c.is_ascii_lowercase(),
                Self::Digit => c.is_ascii_digit(),
                Self::Xdigit => c.is_ascii_hexdigit(),
                Self::Space => matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c'),
                Self::Blank => matches!(c, ' ' | '\t'),
                Self::Cntrl => c.is_ascii_control(),
                Self::Graph => c.is_ascii_graphic(),
                Self::Print => (' '..='~').contains(&c),
                Self::Punct => c.is_ascii_punctuation(),
            };
        }
        let table = match self {
            Self::Alpha | Self::Alnum => tables::ALPHA,
            Self::Upper => tables::UPPER,
            Self::Lower => tables::LOWER,
            Self::Digit | Self::Xdigit => return false,
            Self::Space => tables::SPACE,
            Self::Blank => tables::BLANK,
            Self::Cntrl => tables::CNTRL,
            Self::Punct => tables::PUNCT,
            Self::Graph => return Self::Alpha.contains(c) || Self::Punct.contains(c),
            Self::Print => return Self::Graph.contains(c) || Self::Blank.contains(c),
        };
        in_ranges(table, u32::from(c))
    }
}

fn in_ranges(table: &[(u32, u32)], c: u32) -> bool {
    table
        .binary_search_by(|&(lo, hi)| {
            if c < lo {
                std::cmp::Ordering::Greater
            } else if c > hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

fn mapped(table: &[(u32, u32)], c: char) -> char {
    table
        .binary_search_by_key(&u32::from(c), |&(from, _)| from)
        .ok()
        .and_then(|i| char::from_u32(table[i].1))
        .unwrap_or(c)
}

pub(crate) fn lower(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        mapped(tables::TOLOWER, c)
    }
}

pub(crate) fn upper(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_uppercase()
    } else {
        mapped(tables::TOUPPER, c)
    }
}

/// regcomp's case-insensitive equivalence uses uppercase, not Unicode full folding.
/// Kelvin sign and dotted I have no uppercase partner, unlike fnmatch's lowercase.
pub(crate) fn equal(a: char, b: char, ignore_case: bool) -> bool {
    a == b || (ignore_case && upper(a) == upper(b))
}
