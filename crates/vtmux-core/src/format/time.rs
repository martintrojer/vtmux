//! Time text for formats: `ctime` for `#{t:...}` (format.c:4681-4709,
//! `format_find`) and `strftime` for `format_expand_time` (format.c:6728-6741,
//! used by `display-message`). The broken-down time comes from the host
//! ([`FormatContext::local_time`](super::FormatContext::local_time)), so this
//! module does no I/O and needs no time zone database.

use std::fmt::Write as _;

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
    /// The UTC breakdown of `epoch` (a civil-from-days conversion). Tests and
    /// hosts without a zone use it.
    #[must_use]
    pub fn utc(epoch: i64) -> Self {
        let days = epoch.div_euclid(86_400);
        let secs = epoch.rem_euclid(86_400);
        // Howard Hinnant's civil_from_days.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let cumulative = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
        let month_idx = usize::try_from(month - 1).expect("month 1-12");
        let yearday = cumulative[month_idx] + day - 1 + i64::from(leap && month > 2);
        let small = |v: i64| u8::try_from(v).expect("in range");
        Self {
            year,
            month: small(month),
            day: small(day),
            hour: small(secs / 3600),
            minute: small(secs / 60 % 60),
            second: small(secs % 60),
            weekday: small((days + 4).rem_euclid(7)),
            yearday: u16::try_from(yearday).expect("0-365"),
            utc_offset: 0,
            zone: "UTC".to_owned(),
            epoch,
        }
    }

    fn day_name(&self) -> &'static str {
        DAYS[usize::from(self.weekday % 7)]
    }

    fn month_name(&self) -> &'static str {
        MONTHS[usize::from((self.month + 11) % 12)]
    }

    /// glibc `ctime` without the newline: `%.3s %.3s%3d %.2d:%.2d:%.2d %d`.
    #[must_use]
    pub fn ctime(&self) -> String {
        format!(
            "{} {}{:3} {:02}:{:02}:{:02} {}",
            &self.day_name()[..3],
            &self.month_name()[..3],
            self.day,
            self.hour,
            self.minute,
            self.second,
            self.year
        )
    }

    fn hour12(&self) -> u8 {
        match self.hour % 12 {
            0 => 12,
            h => h,
        }
    }

    /// `strftime` in the C locale for the conversions tmux users write. An
    /// unknown conversion is copied as is, as glibc does.
    #[must_use]
    pub fn strftime(&self, fmt: &str) -> String {
        let mut out = String::new();
        let mut chars = fmt.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            let Some(spec) = chars.next() else {
                out.push('%');
                break;
            };
            self.conversion(spec, &mut out);
        }
        out
    }

    fn conversion(&self, spec: char, out: &mut String) {
        let _ = match spec {
            'a' => write!(out, "{}", &self.day_name()[..3]),
            'A' => write!(out, "{}", self.day_name()),
            'b' | 'h' => write!(out, "{}", &self.month_name()[..3]),
            'B' => write!(out, "{}", self.month_name()),
            'c' => write!(
                out,
                "{} {} {:2} {:02}:{:02}:{:02} {}",
                &self.day_name()[..3],
                &self.month_name()[..3],
                self.day,
                self.hour,
                self.minute,
                self.second,
                self.year
            ),
            'C' => write!(out, "{:02}", self.year.div_euclid(100)),
            'd' => write!(out, "{:02}", self.day),
            'D' | 'x' => write!(
                out,
                "{:02}/{:02}/{:02}",
                self.month,
                self.day,
                self.year.rem_euclid(100)
            ),
            'e' => write!(out, "{:2}", self.day),
            'F' => write!(out, "{}-{:02}-{:02}", self.year, self.month, self.day),
            'H' => write!(out, "{:02}", self.hour),
            'I' => write!(out, "{:02}", self.hour12()),
            'j' => write!(out, "{:03}", self.yearday + 1),
            'k' => write!(out, "{:2}", self.hour),
            'l' => write!(out, "{:2}", self.hour12()),
            'm' => write!(out, "{:02}", self.month),
            'M' => write!(out, "{:02}", self.minute),
            'n' => writeln!(out),
            'p' => write!(out, "{}", if self.hour < 12 { "AM" } else { "PM" }),
            'P' => write!(out, "{}", if self.hour < 12 { "am" } else { "pm" }),
            'r' => write!(
                out,
                "{:02}:{:02}:{:02} {}",
                self.hour12(),
                self.minute,
                self.second,
                if self.hour < 12 { "AM" } else { "PM" }
            ),
            'R' => write!(out, "{:02}:{:02}", self.hour, self.minute),
            's' => write!(out, "{}", self.epoch),
            'S' => write!(out, "{:02}", self.second),
            't' => write!(out, "\t"),
            'T' | 'X' => write!(
                out,
                "{:02}:{:02}:{:02}",
                self.hour, self.minute, self.second
            ),
            'u' => write!(out, "{}", if self.weekday == 0 { 7 } else { self.weekday }),
            'w' => write!(out, "{}", self.weekday),
            'y' => write!(out, "{:02}", self.year.rem_euclid(100)),
            'Y' => write!(out, "{}", self.year),
            'z' => {
                let sign = if self.utc_offset < 0 { '-' } else { '+' };
                let off = self.utc_offset.unsigned_abs();
                write!(out, "{sign}{:02}{:02}", off / 3600, off / 60 % 60)
            }
            'Z' => write!(out, "{}", self.zone),
            '%' => write!(out, "%"),
            other => write!(out, "%{other}"),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference (TZ=UTC): `set -g @t 86400; set -gF @out '#{t:@t}'` gives
    /// `Fri Jan  2 00:00:00 1970`; 1000000000 gives `Sun Sep  9 01:46:40 2001`.
    #[test]
    fn ctime_matches_glibc() {
        assert_eq!(Tm::utc(86_400).ctime(), "Fri Jan  2 00:00:00 1970");
        assert_eq!(Tm::utc(1_000_000_000).ctime(), "Sun Sep  9 01:46:40 2001");
        assert_eq!(Tm::utc(99_999_999_999).ctime(), "Wed Nov 16 09:46:39 5138");
    }

    /// Reference: `date -u -d @1793305246 '+%a %A %b %B %d %e %H %I %j %m %M
    /// %p %S %y %Y %F %T %R %D %u %w %% %Q'`.
    #[test]
    fn strftime_c_locale() {
        let tm = Tm::utc(1_793_305_246);
        assert_eq!(
            tm.strftime("%a %A %b %B %d %e %H %I %j %m %M %p %S %y %Y %F %T %R %D %u %w %% %Q"),
            "Thu Thursday Oct October 29 29 20 08 302 10 20 PM 46 26 2026 2026-10-29 20:20:46 20:20 10/29/26 4 4 % %Q"
        );
        assert_eq!(tm.strftime("x%"), "x%");
        assert_eq!(
            Tm::utc(0).strftime("%l|%k|%I|%s|%z|%Z"),
            "12| 0|12|0|+0000|UTC"
        );
    }
}
