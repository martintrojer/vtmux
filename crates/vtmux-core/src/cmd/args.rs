//! Command arguments: a port of tmux's `args_parse` (arguments.c:256-345).
//!
//! The parser checks only what tmux checks at parse time: flag letters
//! against the command's template, that a value-taking flag has a value, and
//! the positional arity. Values stay raw strings. Numbers, sizes, line
//! references and targets are interpreted when the command runs (`new -d -x
//! -` parses; `new -d -x abc` fails later with `width invalid`), so the
//! accessors below are what the server calls at execution time.

use std::collections::BTreeMap;
use std::fmt;

use super::CmdError;
use super::table::CmdEntry;
use super::target::TargetSpec;
use crate::error::EdgeError;

/// Parsed flags and positionals of one command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    /// Every occurrence of every flag, in order: `Some(value)`, or `None` for
    /// a flag given without a value (booleans, and `x::` flags used bare).
    /// `-pp` is two entries, as `args_has` counts it (arguments.c:663-683).
    pub flags: BTreeMap<char, Vec<Option<String>>>,
    pub positionals: Vec<String>,
}

/// Why a flag's number did not convert (`strtonum`'s `errstr`, plus the
/// `missing` cause of `args_strtonum`, arguments.c:910-935).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumError {
    Missing,
    Invalid,
    TooSmall,
    TooLarge,
}

impl fmt::Display for NumError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Missing => "missing",
            Self::Invalid => "invalid",
            Self::TooSmall => "too small",
            Self::TooLarge => "too large",
        })
    }
}

impl std::error::Error for NumError {}

/// OpenBSD `strtonum` as tmux builds it (compat/strtonum.c): `strtoll` base
/// 10, so leading whitespace and one sign are accepted, the whole string must
/// be consumed, then the range check. `minval > maxval` is invalid.
pub fn strtonum(s: &str, min: i64, max: i64) -> Result<i64, NumError> {
    if min > max {
        return Err(NumError::Invalid);
    }
    let b = s.as_bytes();
    let b = &b[b
        .iter()
        .take_while(|c| matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        .count()..];
    let (negative, digits) = match b.split_first() {
        Some((b'-', d)) => (true, d),
        Some((b'+', d)) => (false, d),
        _ => (false, b),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(NumError::Invalid);
    }
    // Saturate past i64: strtoll reports ERANGE, i.e. too small/large.
    let mut v: i128 = 0;
    for d in digits {
        v = (v * 10 + i128::from(d - b'0')).min(1 << 70);
    }
    let v = if negative { -v } else { v };
    if v < i128::from(min) {
        Err(NumError::TooSmall)
    } else if v > i128::from(max) {
        Err(NumError::TooLarge)
    } else {
        Ok(i64::try_from(v).expect("range checked against i64 bounds"))
    }
}

impl Args {
    /// `args_has`: the flag was given at least once.
    #[must_use]
    pub fn flag(&self, flag: char) -> bool {
        self.flags.contains_key(&flag)
    }

    /// `args_has` as a count: how often the flag was given (`-vv` is 2).
    #[must_use]
    pub fn count(&self, flag: char) -> usize {
        self.flags.get(&flag).map_or(0, Vec::len)
    }

    /// `args_get`: the last value of the flag, if any occurrence had one.
    #[must_use]
    pub fn value(&self, flag: char) -> Option<&str> {
        self.flags
            .get(&flag)?
            .iter()
            .rev()
            .find_map(|v| v.as_deref())
    }

    /// Every value of a repeatable flag (`-e A=1 -e B=2`), in order.
    pub fn values(&self, flag: char) -> impl Iterator<Item = &str> {
        self.flags
            .get(&flag)
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_deref())
    }

    /// `args_string`: positional `idx`.
    #[must_use]
    pub fn positional(&self, idx: usize) -> Option<&str> {
        self.positionals.get(idx).map(String::as_str)
    }

    /// `args_strtonum` without the format expansion: the flag's last value as
    /// a number in `min..=max`. Callers that expand formats first (`capture-pane
    /// -S`, `new -x`) expand `value(flag)` and call [`strtonum`] themselves.
    pub fn number(&self, flag: char, min: i64, max: i64) -> Result<i64, NumError> {
        strtonum(self.value(flag).ok_or(NumError::Missing)?, min, max)
    }

    /// A `-t`/`-s` value as a [`TargetSpec`].
    #[must_use]
    pub fn target(&self, flag: char) -> Option<TargetSpec> {
        self.value(flag).map(TargetSpec::parse)
    }

    /// Port of `args_parse` (arguments.c:256-345) for `words` (the command
    /// name first, as tmux passes it).
    pub fn parse(entry: &CmdEntry, words: &[String]) -> Result<Self, CmdError> {
        let mut args = Self::default();
        let mut i = 1;
        while i < words.len() {
            if !parse_flags(entry, words, &mut i, &mut args)? {
                break;
            }
        }
        // Everything left is positional; `--` was consumed above.
        args.positionals = words.get(i..).unwrap_or_default().to_vec();
        let count = args.positionals.len();
        if count < entry.min_args {
            return Err(EdgeError::TooFewArgs {
                command: entry.name.to_owned(),
                min: entry.min_args,
            }
            .into());
        }
        if let Some(max) = entry.max_args
            && count > max
        {
            return Err(EdgeError::TooManyArgs {
                command: entry.name.to_owned(),
                max,
            }
            .into());
        }
        Ok(args)
    }
}

/// `args_parse_flags` (arguments.c:207-253) for the word at `*i`. Returns
/// `Ok(true)` to keep going and `Ok(false)` when the flags have ended.
fn parse_flags(
    entry: &CmdEntry,
    words: &[String],
    i: &mut usize,
    args: &mut Args,
) -> Result<bool, CmdError> {
    let word = words[*i].as_bytes();
    // A lone "-" or a word not starting with '-' is the first positional.
    if word.first() != Some(&b'-') || word.len() == 1 {
        return Ok(false);
    }
    *i += 1;
    if word == b"--" {
        return Ok(false);
    }
    let name = entry.name.to_owned();
    let mut pos = 1;
    while pos < word.len() {
        let flag = word[pos];
        pos += 1;
        if flag == b'?' {
            // No message: cmd_parse prints `usage: name usage` (cmd.c:520).
            return Err(CmdError::Usage {
                name: entry.name,
                usage: entry.usage,
            });
        }
        if !flag.is_ascii_alphanumeric() {
            return Err(CmdError::InvalidFlag {
                command: name,
                flag,
            });
        }
        let ch = char::from(flag);
        let Some(at) = entry.template.find(ch) else {
            return Err(EdgeError::UnknownFlag {
                command: name,
                flag: ch,
            }
            .into());
        };
        let template = entry.template.as_bytes();
        if template.get(at + 1) != Some(&b':') {
            args.flags.entry(ch).or_default().push(None);
            continue;
        }
        let optional = template.get(at + 2) == Some(&b':');
        // The rest of this word is the value (`-ta`); `pos` is on a char
        // boundary because flag letters are ASCII.
        let rest = &words[*i - 1][pos..];
        let value = flag_argument(entry, words, i, rest, ch, optional)?;
        args.flags.entry(ch).or_default().push(value);
        return Ok(true);
    }
    Ok(true)
}

/// `args_parse_flag_argument` (arguments.c:147-204): the value of a flag that
/// takes one. `None` is an optional flag given without a value.
fn flag_argument(
    entry: &CmdEntry,
    words: &[String],
    i: &mut usize,
    rest: &str,
    flag: char,
    optional: bool,
) -> Result<Option<String>, CmdError> {
    if !rest.is_empty() {
        return Ok(Some(rest.to_owned()));
    }
    let Some(next) = words.get(*i) else {
        if optional {
            return Ok(None); // "either - or end"
        }
        return Err(CmdError::FlagNeedsValue {
            command: entry.name.to_owned(),
            flag,
        });
    };
    if optional {
        // An optional value never swallows something that looks like a flag.
        let b = next.as_bytes();
        if b.first() == Some(&b'-')
            && b.get(1)
                .is_some_and(|c| *c == b'-' || c.is_ascii_alphabetic())
        {
            return Ok(None);
        }
    }
    *i += 1;
    Ok(Some(next.clone()))
}
