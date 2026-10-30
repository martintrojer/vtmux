//! Wall-clock time for formats: the broken-down local time (`struct tm`), `ctime`
//! and a port of glibc's `strftime` (the reference tmux calls `localtime_r`,
//! `ctime_r` and `strftime`, format.c:4694-4700, 6728-6741), and the injectable
//! [`WallClock`] that says what time it is and which zone is local.
//!
//! The C locale only: tmux calls `setlocale(LC_TIME, "")`, so a non-C `LANG`
//! changes day and month names there (`compat/DIVERGENCES.md`, D-004).

use jiff::{Timestamp, tz::TimeZone};

/// A broken-down local time (`struct tm` plus the zone fields glibc adds).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tm {
    /// Full year, e.g. 2026.
    pub year: i64,
    /// Month, 1-12.
    pub month: u8,
    /// Day of the month, 1-31.
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    /// Day of the week, 0 = Sunday.
    pub weekday: u8,
    /// Day of the year, 0-365.
    pub yearday: u16,
    /// Seconds east of UTC.
    pub utc_offset: i32,
    /// Zone abbreviation (`%Z`).
    pub zone: String,
    /// Seconds since the epoch (`%s`).
    pub epoch: i64,
}

/// What time it is and where: the source of `#{t:...}` and of `strftime` in
/// `display-message` formats. Injected like [`Clock`](crate::format::Clock) so
/// tests can pin both the instant and the zone.
pub trait WallClock {
    /// Seconds since the Unix epoch now.
    fn now(&self) -> i64;
    /// `localtime_r`: `epoch` broken down in the local zone.
    fn local_time(&self, epoch: i64) -> Option<Tm>;
}

/// The real clock in the process's zone (`TZ`, else `/etc/localtime`).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemWallClock;

impl WallClock for SystemWallClock {
    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }

    fn local_time(&self, epoch: i64) -> Option<Tm> {
        Tm::in_zone(&TimeZone::system(), epoch)
    }
}

/// A clock stuck at one instant in one named zone.
#[derive(Debug, Clone)]
pub struct FixedWallClock {
    now: i64,
    tz: TimeZone,
}

impl FixedWallClock {
    /// `tz` is an IANA name (`Europe/London`, `UTC`) from the host's zoneinfo
    /// database; `None` when it is unknown.
    #[must_use]
    pub fn new(now: i64, tz: &str) -> Option<Self> {
        Some(Self {
            now,
            tz: TimeZone::get(tz).ok()?,
        })
    }
}

impl WallClock for FixedWallClock {
    fn now(&self) -> i64 {
        self.now
    }

    fn local_time(&self, epoch: i64) -> Option<Tm> {
        Tm::in_zone(&self.tz, epoch)
    }
}

const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

impl Tm {
    /// `epoch` in `tz`; `None` outside the range the zone code supports.
    #[must_use]
    pub fn in_zone(tz: &TimeZone, epoch: i64) -> Option<Self> {
        let zoned = Timestamp::from_second(epoch).ok()?.to_zoned(tz.clone());
        let info = tz.to_offset_info(zoned.timestamp());
        Some(Self {
            year: i64::from(zoned.year()),
            month: u8::try_from(zoned.month()).ok()?,
            day: u8::try_from(zoned.day()).ok()?,
            hour: u8::try_from(zoned.hour()).ok()?,
            minute: u8::try_from(zoned.minute()).ok()?,
            second: u8::try_from(zoned.second()).ok()?,
            weekday: u8::try_from(zoned.weekday().to_sunday_zero_offset()).ok()?,
            yearday: u16::try_from(zoned.day_of_year() - 1).ok()?,
            utc_offset: info.offset().seconds(),
            zone: info.abbreviation().to_owned(),
            epoch,
        })
    }

    /// The UTC breakdown of `epoch`.
    ///
    /// # Panics
    /// When `epoch` is outside years -9999..=9999.
    #[must_use]
    pub fn utc(epoch: i64) -> Self {
        Self::in_zone(&TimeZone::UTC, epoch).expect("epoch within the supported years")
    }

    /// glibc `ctime_r` without the newline: `%.3s %.3s%3d %.2d:%.2d:%.2d %d`.
    /// `None` when the year needs more than 4 digits: the result would not fit
    /// the 26-byte buffer and `asctime_r` fails with `EOVERFLOW`.
    #[must_use]
    pub fn ctime(&self) -> Option<String> {
        if !(0..=9999).contains(&self.year) {
            return None;
        }
        Some(format!(
            "{} {}{:3} {:02}:{:02}:{:02} {}",
            &DAYS[usize::from(self.weekday % 7)][..3],
            &MONTHS[usize::from((self.month + 11) % 12)][..3],
            self.day,
            self.hour,
            self.minute,
            self.second,
            self.year
        ))
    }

    /// `strftime` with no size limit.
    #[must_use]
    pub fn strftime(&self, fmt: &str) -> String {
        self.strftime_max(fmt, usize::MAX).unwrap_or_default()
    }

    /// `strftime(s, max, fmt, tm)` (glibc `time/strftime_l.c`, C locale): the
    /// text, or `None` when it does not fit in `max` bytes including the NUL,
    /// where glibc returns 0. tmux gives 8192 bytes for a template
    /// (format.c:6733) and 512 for a `t/f/...` argument (format.c:4700).
    #[must_use]
    pub fn strftime_max(&self, fmt: &str, max: usize) -> Option<String> {
        let mut out = Out::new(max);
        self.format_into(&mut out, fmt.as_bytes())?;
        Some(String::from_utf8_lossy(&out.buf).into_owned())
    }
}

/// The output buffer of one `strftime` call: `maxsize - i` in glibc's checks.
struct Out {
    buf: Vec<u8>,
    max: usize,
}

/// The flags of one conversion (glibc's `pad`, `width`, case variables).
#[derive(Clone, Copy)]
struct Spec {
    /// `_`, `-`, `0` or 0 for none.
    pad: u8,
    /// `-1` when no width was written.
    width: i64,
    upper: bool,
    lower: bool,
    change_case: bool,
    /// `E`, `O` or 0.
    modifier: u8,
}

impl Out {
    fn new(max: usize) -> Self {
        Self {
            buf: Vec::new(),
            max,
        }
    }

    /// The `add` macro: `n` bytes of `text` (already case-converted), padded
    /// on the left to the width. `None` when it would not fit.
    fn add(&mut self, text: &[u8], spec: &Spec) -> Option<()> {
        let n = i64::try_from(text.len()).ok()?;
        let delta = (spec.width - n).max(0);
        let incr = usize::try_from(n + delta).ok()?;
        if incr >= self.max.saturating_sub(self.buf.len()) {
            return None;
        }
        let fill = if spec.pad == b'0' { b'0' } else { b' ' };
        self.buf
            .extend(std::iter::repeat_n(fill, usize::try_from(delta).ok()?));
        self.buf.extend_from_slice(text);
        Some(())
    }

    /// The `cpy` macro: `add` with the case flags applied.
    fn cpy(&mut self, text: &[u8], spec: &Spec) -> Option<()> {
        if spec.lower {
            self.add(&text.to_ascii_lowercase(), spec)
        } else if spec.upper {
            self.add(&text.to_ascii_uppercase(), spec)
        } else {
            self.add(text, spec)
        }
    }

    /// `do_number_sign_and_padding`: `digits` is the minimum digit count.
    fn number(&mut self, mut spec: Spec, digits: i64, value: i64) -> Option<()> {
        let negative = value < 0;
        let mut text = value.unsigned_abs().to_string().into_bytes();
        if negative {
            text.insert(0, b'-');
        }
        self.sign_and_padding(&mut spec, digits, negative, &text)
    }

    fn sign_and_padding(
        &mut self,
        spec: &mut Spec,
        digits: i64,
        negative: bool,
        text: &[u8],
    ) -> Option<()> {
        let mut text = text;
        if spec.pad != b'-' {
            let padding = digits - i64::try_from(text.len()).ok()?;
            if padding > 0 {
                let count = usize::try_from(padding).ok()?;
                if spec.pad == b'_' {
                    if count >= self.max.saturating_sub(self.buf.len()) {
                        return None;
                    }
                    self.buf.extend(std::iter::repeat_n(b' ', count));
                    spec.width = if spec.width > padding {
                        spec.width - padding
                    } else {
                        0
                    };
                } else {
                    if usize::try_from(digits).ok()? >= self.max.saturating_sub(self.buf.len()) {
                        return None;
                    }
                    if negative {
                        text = &text[1..];
                        self.buf.push(b'-');
                    }
                    self.buf.extend(std::iter::repeat_n(b'0', count));
                    spec.width = 0;
                }
            }
        }
        self.add(text, spec)
    }
}

impl Tm {
    fn hour12(&self) -> i64 {
        match self.hour % 12 {
            0 => 12,
            h => i64::from(h),
        }
    }

    /// glibc's `iso_week_days`.
    fn iso_week_days(yday: i64, wday: i64) -> i64 {
        // (-YDAY_MINIMUM / 7 + 2) * 7 with YDAY_MINIMUM -366.
        let big_enough_multiple_of_7 = (366 / 7 + 2) * 7;
        yday - (yday - wday + 4 + big_enough_multiple_of_7) % 7 + 4 - 1
    }

    fn format_into(&self, out: &mut Out, fmt: &[u8]) -> Option<()> {
        let mut f = 0;
        while f < fmt.len() {
            if fmt[f] != b'%' {
                out.add(&fmt[f..=f], &Spec::plain())?;
                f += 1;
                continue;
            }
            let start = f;
            let mut spec = Spec::plain();
            // Flags.
            loop {
                f += 1;
                match fmt.get(f) {
                    Some(&c @ (b'_' | b'-' | b'0')) => spec.pad = c,
                    Some(b'^') => spec.upper = true,
                    Some(b'#') => spec.change_case = true,
                    _ => break,
                }
            }
            // Width (a GNU extension), saturating at INT_MAX.
            if fmt.get(f).is_some_and(u8::is_ascii_digit) {
                spec.width = 0;
                while let Some(d) = fmt.get(f).filter(|d| d.is_ascii_digit()) {
                    spec.width = (spec.width * 10 + i64::from(d - b'0')).min(i64::from(i32::MAX));
                    f += 1;
                }
            }
            if let Some(&m @ (b'E' | b'O')) = fmt.get(f) {
                spec.modifier = m;
                f += 1;
            }
            // `\0` is the end of the format: glibc steps back onto the last byte.
            let at_end = f >= fmt.len();
            if at_end {
                f -= 1;
            }
            let c = if at_end { 0 } else { fmt[f] };
            if self.conversion(out, c, spec)?.is_none() {
                // bad_format: the text from the nearest `%` through here.
                let from = fmt[..=f].iter().rposition(|&b| b == b'%').unwrap_or(start);
                out.cpy(&fmt[from..=f], &spec)?;
            }
            f += 1;
        }
        Some(())
    }

    /// One conversion. The outer `None` is "does not fit"; `Some(None)` is
    /// glibc's `bad_format`.
    #[allow(
        clippy::too_many_lines,
        clippy::many_single_char_names,
        clippy::option_option,
        clippy::match_same_arms
    )]
    fn conversion(&self, out: &mut Out, c: u8, mut spec: Spec) -> Option<Option<()>> {
        let e = spec.modifier == b'E';
        let o = spec.modifier == b'O';
        let any = spec.modifier != 0;
        let year = self.year;
        let wday = i64::from(self.weekday);
        let yday = i64::from(self.yearday);
        let text = |out: &mut Out, s: &str, spec: &Spec| out.cpy(s.as_bytes(), spec).map(Some);
        let sub = |out: &mut Out, fmt: &str, spec: &Spec| {
            let mut inner = Out::new(usize::MAX);
            self.format_into(&mut inner, fmt.as_bytes())?;
            if spec.upper {
                inner.buf.make_ascii_uppercase();
            }
            out.add(&inner.buf, spec).map(Some)
        };
        let num = |out: &mut Out, spec: Spec, digits: i64, value: i64, space: bool| {
            let mut spec = spec;
            if space && spec.pad != b'0' && spec.pad != b'-' {
                spec.pad = b'_';
            }
            out.number(spec, digits.max(spec.width), value).map(Some)
        };
        match c {
            b'%' if any => Some(None),
            b'%' => out.add(b"%", &spec).map(Some),
            b'a' | b'A' if any => Some(None),
            b'a' | b'A' | b'b' | b'B' | b'h' => {
                if (matches!(c, b'b' | b'B' | b'h')) && e {
                    return Some(None);
                }
                if spec.change_case {
                    spec.upper = true;
                    spec.lower = false;
                }
                let name = match c {
                    b'a' => &DAYS[usize::from(self.weekday % 7)][..3],
                    b'A' => DAYS[usize::from(self.weekday % 7)],
                    b'B' => MONTHS[usize::from((self.month + 11) % 12)],
                    _ => &MONTHS[usize::from((self.month + 11) % 12)][..3],
                };
                text(out, name, &spec)
            }
            b'c' if o => Some(None),
            b'c' => sub(out, "%a %b %e %H:%M:%S %Y", &spec),
            b'C' => num(out, spec, 1, year / 100 - i64::from(year % 100 < 0), false),
            b'x' if o => Some(None),
            b'x' => sub(out, "%m/%d/%y", &spec),
            b'D' if any => Some(None),
            b'D' => sub(out, "%m/%d/%y", &spec),
            b'd' | b'H' | b'I' | b'j' | b'M' | b'm' | b'S' | b'U' | b'W' | b'w' if e => Some(None),
            b'e' | b'k' | b'l' if e => Some(None),
            b'd' => num(out, spec, 2, i64::from(self.day), false),
            b'e' => num(out, spec, 2, i64::from(self.day), true),
            b'F' if any => Some(None),
            b'F' => sub(out, "%Y-%m-%d", &spec),
            b'H' => num(out, spec, 2, i64::from(self.hour), false),
            b'I' => num(out, spec, 2, self.hour12(), false),
            b'k' => num(out, spec, 2, i64::from(self.hour), true),
            b'l' => num(out, spec, 2, self.hour12(), true),
            b'j' => num(out, spec, 3, 1 + yday, false),
            b'M' => num(out, spec, 2, i64::from(self.minute), false),
            b'm' => num(out, spec, 2, i64::from(self.month), false),
            b'n' => out.add(b"\n", &spec).map(Some),
            b'P' | b'p' => {
                if c == b'P' {
                    spec.lower = true;
                }
                if spec.change_case {
                    spec.upper = false;
                    spec.lower = true;
                }
                text(out, if self.hour > 11 { "PM" } else { "AM" }, &spec)
            }
            b'R' => sub(out, "%H:%M", &spec),
            b'r' => sub(out, "%I:%M:%S %p", &spec),
            b'S' => num(out, spec, 2, i64::from(self.second), false),
            b's' => out.number(spec, 1, self.epoch).map(Some),
            b'X' if o => Some(None),
            b'X' | b'T' => sub(out, "%H:%M:%S", &spec),
            b't' => out.add(b"\t", &spec).map(Some),
            b'u' => num(out, spec, 1, (wday - 1 + 7) % 7 + 1, false),
            b'U' => num(out, spec, 2, (yday - wday + 7) / 7, false),
            b'V' | b'g' | b'G' if e => Some(None),
            b'V' | b'g' | b'G' => {
                let leap = |y: i64| 365 + i64::from(y % 4 == 0 && (y % 100 != 0 || y % 400 == 0));
                let mut y = year;
                let mut days = Self::iso_week_days(yday, wday);
                if days < 0 {
                    // This ISO week belongs to the previous year.
                    y -= 1;
                    days = Self::iso_week_days(yday + leap(y), wday);
                } else {
                    let d = Self::iso_week_days(yday - leap(y), wday);
                    if d >= 0 {
                        y += 1;
                        days = d;
                    }
                }
                match c {
                    b'g' => num(out, spec, 2, (y % 100 + 100) % 100, false),
                    b'G' => num(out, spec, 1, y, false),
                    _ => num(out, spec, 2, days / 7 + 1, false),
                }
            }
            b'W' => num(out, spec, 2, (yday - (wday - 1 + 7) % 7 + 7) / 7, false),
            b'w' => num(out, spec, 1, wday, false),
            b'Y' if o => Some(None),
            b'Y' => num(out, spec, 1, year, false),
            b'y' => num(out, spec, 2, year.rem_euclid(100), false),
            b'Z' => {
                if spec.change_case {
                    spec.upper = false;
                    spec.lower = true;
                }
                text(out, &self.zone, &spec)
            }
            b'z' => {
                let sign: &[u8] = if self.utc_offset < 0 { b"-" } else { b"+" };
                out.add(sign, &spec)?;
                let minutes = i64::from(self.utc_offset.unsigned_abs()) / 60;
                num(out, spec, 4, (minutes / 60) * 100 + minutes % 60, false)
            }
            _ => Some(None),
        }
    }
}

impl Spec {
    fn plain() -> Self {
        Self {
            pad: 0,
            width: -1,
            upper: false,
            lower: false,
            change_case: false,
            modifier: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference (TZ=UTC): `set -g @t 86400; set -gF @out '#{t:@t}'` gives
    /// `Fri Jan  2 00:00:00 1970`; 1000000000 gives `Sun Sep  9 01:46:40 2001`.
    #[test]
    fn ctime_matches_glibc() {
        assert_eq!(
            Tm::utc(86_400).ctime().as_deref(),
            Some("Fri Jan  2 00:00:00 1970")
        );
        assert_eq!(
            Tm::utc(1_000_000_000).ctime().as_deref(),
            Some("Sun Sep  9 01:46:40 2001")
        );
        assert_eq!(
            Tm::utc(99_999_999_999).ctime().as_deref(),
            Some("Wed Nov 16 09:46:39 5138")
        );
    }

    #[test]
    fn zones_come_from_the_database() {
        let london = FixedWallClock::new(0, "Europe/London").expect("zoneinfo");
        let winter = london.local_time(1_793_292_783).expect("in range");
        assert_eq!((winter.zone.as_str(), winter.utc_offset), ("GMT", 0));
        let summer = london.local_time(1_782_000_000).expect("in range");
        assert_eq!((summer.zone.as_str(), summer.utc_offset), ("BST", 3600));
        assert!(FixedWallClock::new(0, "No/Such_Zone").is_none());
    }

    #[test]
    fn output_limit_is_glibc_maxsize() {
        let tm = Tm::utc(0);
        // 8191 bytes fit in 8192 (the NUL takes the last); 8192 do not.
        assert_eq!(
            tm.strftime_max(&"x".repeat(8191), 8192).map(|s| s.len()),
            Some(8191)
        );
        assert_eq!(tm.strftime_max(&"x".repeat(8192), 8192), None);
        assert_eq!(tm.strftime_max("%999999999d", 512), None);
    }
}
