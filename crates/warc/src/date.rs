//! `WarcDate`: the UTC timestamp of a WARC record, without any date dependency.

use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::ParseError;

/// UTC instant with nanosecond precision, year 0000..=9999.
///
/// Invariant: the fields form a valid Gregorian date/time (second `0..=60` to admit leap seconds,
/// nanos `< 1e9`). The field order makes the derived `Ord` chronological.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WarcDate {
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    nanos: u32,
}

fn is_leap(year: u16) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn days_in_month(year: u16, month: u8) -> Option<u8> {
    Some(match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => return None,
    })
}

/// Days since 1970-01-01 to (year, month, day), Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    // m in 1..=12, d in 1..=31 by construction of the algorithm.
    (
        y,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

/// Value of a non-empty run of ASCII digits (at most 9, so it fits in a `u32`).
fn digits(b: &[u8]) -> Option<u32> {
    if b.is_empty() || b.len() > 9 || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(b.iter().fold(0u32, |n, &d| n * 10 + u32::from(d - b'0')))
}

impl WarcDate {
    /// Validating constructor; `None` if any field is out of range (the day is checked against the
    /// month and the leap year; `year` must be `<= 9999`, `second <= 60`, `nanos < 1_000_000_000`).
    pub fn new(
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
        nanos: u32,
    ) -> Option<Self> {
        if year > 9999
            || day == 0
            || day > days_in_month(year, month)?
            || hour > 23
            || minute > 59
            || second > 60
            || nanos >= 1_000_000_000
        {
            return None;
        }
        Some(Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
            nanos,
        })
    }

    /// Accepts exactly `YYYY-MM-DDThh:mm:ssZ` or `YYYY-MM-DDThh:mm:ss.<1..=9 digits>Z`
    /// (W3C-ISO8601 profile of WARC 1.1, a subset of RFC 3339).
    ///
    /// # Errors
    /// `ParseError::InvalidDate` for anything else, including out-of-range fields.
    pub fn parse(s: &str) -> Result<Self, ParseError> {
        Self::parse_opt(s.as_bytes()).ok_or(ParseError::InvalidDate)
    }

    fn parse_opt(b: &[u8]) -> Option<Self> {
        let (head, rest) = b.split_at_checked(19)?;
        let sep = |i: usize, c: u8| (head.get(i) == Some(&c)).then_some(());
        sep(4, b'-')?;
        sep(7, b'-')?;
        sep(10, b'T')?;
        sep(13, b':')?;
        sep(16, b':')?;
        let num = |from: usize, to: usize| digits(head.get(from..to)?);
        let year = u16::try_from(num(0, 4)?).ok()?;
        let month = u8::try_from(num(5, 7)?).ok()?;
        let day = u8::try_from(num(8, 10)?).ok()?;
        let hour = u8::try_from(num(11, 13)?).ok()?;
        let minute = u8::try_from(num(14, 16)?).ok()?;
        let second = u8::try_from(num(17, 19)?).ok()?;
        let nanos = match rest {
            [b'Z'] => 0,
            [b'.', frac @ .., b'Z'] => {
                let n = digits(frac)?;
                // frac.len() is 1..=9 (checked by `digits`).
                let scale = 10u32.pow(u32::try_from(9 - frac.len()).ok()?);
                n * scale
            }
            _ => return None,
        };
        Self::new(year, month, day, hour, minute, second, nanos)
    }

    /// `None` if before 1970-01-01 or after 9999-12-31T23:59:59.999999999Z.
    pub fn from_system_time(t: SystemTime) -> Option<Self> {
        let d = t.duration_since(UNIX_EPOCH).ok()?;
        let secs = i64::try_from(d.as_secs()).ok()?;
        let (y, m, day) = civil_from_days(secs.div_euclid(86_400));
        let sod = secs.rem_euclid(86_400);
        let year = u16::try_from(y).ok().filter(|&y| y <= 9999)?;
        Self::new(
            year,
            u8::try_from(m).ok()?,
            u8::try_from(day).ok()?,
            u8::try_from(sod / 3600).ok()?,
            u8::try_from(sod % 3600 / 60).ok()?,
            u8::try_from(sod % 60).ok()?,
            d.subsec_nanos(),
        )
    }

    /// Year, 0..=9999.
    pub fn year(&self) -> u16 {
        self.year
    }
    /// Month, 1..=12.
    pub fn month(&self) -> u8 {
        self.month
    }
    /// Day of month, 1..=31.
    pub fn day(&self) -> u8 {
        self.day
    }
    /// Hour, 0..=23.
    pub fn hour(&self) -> u8 {
        self.hour
    }
    /// Minute, 0..=59.
    pub fn minute(&self) -> u8 {
        self.minute
    }
    /// Second, 0..=60 (60 is a leap second).
    pub fn second(&self) -> u8 {
        self.second
    }
    /// Nanoseconds, `< 1_000_000_000`.
    pub fn nanos(&self) -> u32 {
        self.nanos
    }
}

/// `YYYY-MM-DDThh:mm:ss[.f]Z`; the fraction is omitted when `nanos == 0`, else trailing zeros are trimmed.
impl fmt::Display for WarcDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )?;
        if self.nanos != 0 {
            let mut n = self.nanos;
            let mut w = 9usize;
            while n.is_multiple_of(10) {
                n /= 10;
                w -= 1;
            }
            write!(f, ".{n:0w$}")?;
        }
        f.write_str("Z")
    }
}

impl FromStr for WarcDate {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn date_parse_ok() {
        for s in [
            "2026-11-12T10:00:00Z",
            "2000-02-29T23:59:60Z",
            "2026-11-12T10:00:00.1Z",
            "2026-11-12T10:00:00.123456789Z",
        ] {
            assert!(WarcDate::parse(s).is_ok(), "{s}");
        }
        let d = WarcDate::parse("2026-11-12T10:00:00.123456789Z").unwrap();
        assert_eq!(
            (
                d.year(),
                d.month(),
                d.day(),
                d.hour(),
                d.minute(),
                d.second()
            ),
            (2026, 11, 12, 10, 0, 0)
        );
        assert_eq!(d.nanos(), 123_456_789);
    }

    #[test]
    fn date_parse_rejects() {
        for s in [
            "2026-11-12T10:00:00",
            "2026-11-12T10:00:00+02:00",
            "2026-11-12t10:00:00Z",
            "2026-11-12T10:00:00z",
            "2026-02-29T10:00:00Z",
            "1900-02-29T10:00:00Z",
            "2026-13-01T10:00:00Z",
            "2026-11-12T24:00:00Z",
            "2026-11-12T10:00:00.1234567890Z",
            "2026-11-12T10:00:00.Z",
            "2026-11-12T10:00:00Z ",
            "2026-1-01T10:00:00Z",
            "２０２６-11-12T10:00:00Z",
            "",
            "2026-00-12T10:00:00Z",
            "2026-11-00T10:00:00Z",
            "2026-11-12T10:60:00Z",
            "2026-11-12T10:00:61Z",
            "2026-11-12T10:00:00.1",
        ] {
            assert_eq!(WarcDate::parse(s), Err(ParseError::InvalidDate), "{s}");
        }
    }

    #[test]
    fn date_display_roundtrip() {
        for s in [
            "2026-11-12T10:00:00Z",
            "2026-11-12T10:00:00.1Z",
            "2026-11-12T10:00:00.123456Z",
            "2026-11-12T10:00:00.000000001Z",
            "0000-01-01T00:00:00Z",
        ] {
            assert_eq!(WarcDate::parse(s).unwrap().to_string(), s);
            assert_eq!(s.parse::<WarcDate>().unwrap().to_string(), s);
        }
        assert_eq!(
            WarcDate::parse("2026-11-12T10:00:00.500Z")
                .unwrap()
                .to_string(),
            "2026-11-12T10:00:00.5Z"
        );
    }

    #[test]
    fn date_from_system_time() {
        let at = |s: u64, ns: u32| WarcDate::from_system_time(UNIX_EPOCH + Duration::new(s, ns));
        assert_eq!(at(0, 0).unwrap().to_string(), "1970-01-01T00:00:00Z");
        assert_eq!(
            at(951_782_400, 500_000_000).unwrap().to_string(),
            "2000-02-29T00:00:00.5Z"
        );
        assert_eq!(
            at(1_794_531_661, 0).unwrap().to_string(),
            "2026-11-13T01:01:01Z"
        );
        assert_eq!(
            WarcDate::from_system_time(UNIX_EPOCH - Duration::from_secs(1)),
            None
        );
        // 10000-01-01T00:00:00Z
        assert_eq!(at(253_402_300_800, 0), None);
        assert_eq!(
            at(253_402_300_799, 999_999_999).unwrap().to_string(),
            "9999-12-31T23:59:59.999999999Z"
        );
    }

    #[test]
    fn date_ord_chronological() {
        let expected = [
            "0999-12-31T23:59:59Z",
            "2026-11-12T10:00:00Z",
            "2026-11-12T10:00:00.1Z",
            "2026-11-12T10:00:01Z",
            "2026-12-01T00:00:00Z",
            "2027-01-01T00:00:00Z",
        ];
        let mut v: Vec<_> = expected
            .iter()
            .rev()
            .map(|s| WarcDate::parse(s).unwrap())
            .collect();
        v.sort();
        let got: Vec<_> = v.iter().map(ToString::to_string).collect();
        assert_eq!(got, expected);
    }
}
