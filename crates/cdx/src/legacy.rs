//! Read-only parser of the legacy CDX line (spec 3.4), for import and migration.

use masstuffy_warc::WarcDate;

/// A bad legacy line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LegacyError {
    /// The line is not UTF-8.
    #[error("line is not valid UTF-8")]
    NotUtf8,
    /// The line contains a byte `< 0x20` or `0x7F`.
    #[error("control byte in line")]
    ControlByte,
    /// Fewer than 7 fields, or an empty field.
    #[error("wrong number of fields")]
    FieldCount,
    /// A required field is `-` (the field name).
    #[error("missing field `{0}`")]
    Missing(&'static str),
    /// The date is not `YYYYmmddHHMMSS` or is out of range.
    #[error("invalid date")]
    InvalidDate,
    /// A number is not 1 to 20 digits or overflows `u64` (the field name).
    #[error("invalid number in field `{0}`")]
    InvalidNumber(&'static str),
    /// `raw_size` is zero.
    #[error("raw_size is zero")]
    ZeroRawSize,
}

/// A parsed legacy line: `<url> <type> <record_id> <date> <filename> <offset> <raw_size>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyEntry<'a> {
    /// Target URI; `-` is `None`. May contain spaces.
    pub url: Option<&'a str>,
    /// `WARC-Type`.
    pub warc_type: &'a str,
    /// `WARC-Record-ID` without `<>`.
    pub record_id: &'a str,
    /// Date (`YYYYmmddHHMMSS`, UTC, nanos 0).
    pub date: WarcDate,
    /// File name; `-` is `None`. Ignored by the import envelope.
    pub filename: Option<&'a str>,
    /// Offset (leading zeros accepted); `-` is `None`. Ignored by the import envelope.
    pub offset: Option<u64>,
    /// Stored size, at least 1 (leading zeros accepted).
    pub raw_size: u64,
}

/// Lenient decimal: 1 to 20 digits, leading zeros allowed.
fn lenient_u64(s: &str, name: &'static str) -> Result<u64, LegacyError> {
    if s.is_empty() || s.len() > 20 {
        return Err(LegacyError::InvalidNumber(name));
    }
    s.bytes()
        .try_fold(0u64, |n, d| {
            if !d.is_ascii_digit() {
                return None;
            }
            n.checked_mul(10)?.checked_add(u64::from(d - b'0'))
        })
        .ok_or(LegacyError::InvalidNumber(name))
}

fn atom<'a>(s: &'a str, name: &'static str) -> Result<&'a str, LegacyError> {
    if s == "-" {
        return Err(LegacyError::Missing(name));
    }
    // Control bytes were rejected for the whole line and fields hold no space.
    Ok(s)
}

fn parse_date(s: &str) -> Result<WarcDate, LegacyError> {
    if s == "-" {
        return Err(LegacyError::Missing("date"));
    }
    let b = s.as_bytes();
    if b.len() != 14 || !b.iter().all(u8::is_ascii_digit) {
        return Err(LegacyError::InvalidDate);
    }
    let num = |from: usize, to: usize| -> Option<u32> {
        b.get(from..to)?
            .iter()
            .try_fold(0u32, |n, &d| Some(n * 10 + u32::from(d - b'0')))
    };
    let f = || {
        WarcDate::new(
            u16::try_from(num(0, 4)?).ok()?,
            u8::try_from(num(4, 6)?).ok()?,
            u8::try_from(num(6, 8)?).ok()?,
            u8::try_from(num(8, 10)?).ok()?,
            u8::try_from(num(10, 12)?).ok()?,
            u8::try_from(num(12, 14)?).ok()?,
            0,
        )
    };
    f().ok_or(LegacyError::InvalidDate)
}

/// Parses one legacy line **without** its `\n`.
///
/// The line must be UTF-8 with no byte `< 0x20` and no `0x7F`. The 6 rightmost fields contain no
/// spaces, so `url` is everything before them and may contain spaces. Every field is non-empty.
///
/// # Errors
/// See [`LegacyError`].
pub fn parse_line(line: &[u8]) -> Result<LegacyEntry<'_>, LegacyError> {
    let s = std::str::from_utf8(line).map_err(|_| LegacyError::NotUtf8)?;
    if s.bytes().any(|b| b < 0x20 || b == 0x7F) {
        return Err(LegacyError::ControlByte);
    }
    let mut it = s.rsplitn(7, ' ');
    let mut next = || {
        it.next()
            .filter(|f| !f.is_empty())
            .ok_or(LegacyError::FieldCount)
    };
    let raw_size = next()?;
    let offset = next()?;
    let filename = next()?;
    let date = next()?;
    let record_id = next()?;
    let warc_type = next()?;
    let url = next()?;

    if raw_size == "-" {
        return Err(LegacyError::Missing("raw_size"));
    }
    let raw_size = lenient_u64(raw_size, "raw_size")?;
    if raw_size == 0 {
        return Err(LegacyError::ZeroRawSize);
    }
    let offset = if offset == "-" {
        None
    } else {
        Some(lenient_u64(offset, "offset")?)
    };
    Ok(LegacyEntry {
        url: (url != "-").then_some(url),
        warc_type: atom(warc_type, "warc_type")?,
        record_id: atom(record_id, "record_id")?,
        date: parse_date(date)?,
        filename: (filename != "-").then_some(filename),
        offset,
        raw_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "urn:uuid:0f3e5a1c-0000-4000-8000-000000000000";

    fn p(s: &str) -> Result<LegacyEntry<'_>, LegacyError> {
        parse_line(s.as_bytes())
    }

    #[test]
    fn legacy_spec_example() {
        let s = format!("https://example.com/a response {ID} 20250503114301 seg.1.warc 48213 1532");
        let e = p(&s).unwrap();
        assert_eq!(e.url, Some("https://example.com/a"));
        assert_eq!(e.warc_type, "response");
        assert_eq!(e.record_id, ID);
        assert_eq!(e.date.to_string(), "2025-05-03T11:43:01Z");
        assert_eq!(e.filename, Some("seg.1.warc"));
        assert_eq!(e.offset, Some(48213));
        assert_eq!(e.raw_size, 1532);
    }

    #[test]
    fn legacy_url_absent_and_spaces() {
        let s = format!("- response {ID} 20250503114301 - - 5");
        let e = p(&s).unwrap();
        assert_eq!(e.url, None);
        let s = format!("http://a/b c d response {ID} 20250503114301 f 1 5");
        let e = p(&s).unwrap();
        assert_eq!(e.url, Some("http://a/b c d"));
    }

    #[test]
    fn legacy_rejects() {
        let t = "20250503114301";
        assert_eq!(
            p(&format!("u response {ID} {t} f 1")),
            Err(LegacyError::FieldCount)
        );
        assert_eq!(
            p(&format!("u - {ID} {t} f 1 5")),
            Err(LegacyError::Missing("warc_type"))
        );
        assert_eq!(
            p(&format!("u response - {t} f 1 5")),
            Err(LegacyError::Missing("record_id"))
        );
        assert_eq!(
            p(&format!("u response {ID} - f 1 5")),
            Err(LegacyError::Missing("date"))
        );
        assert_eq!(
            p(&format!("u response {ID} {t} f 1 -")),
            Err(LegacyError::Missing("raw_size"))
        );
        assert_eq!(
            p(&format!("u response {ID} {t} f 1 0")),
            Err(LegacyError::ZeroRawSize)
        );
        for d in ["20250230000000", "2025050311430", "2025050311430a"] {
            assert_eq!(
                p(&format!("u response {ID} {d} f 1 5")),
                Err(LegacyError::InvalidDate),
                "{d}"
            );
        }
        assert_eq!(
            p(&format!("u response {ID} {t} f x 5")),
            Err(LegacyError::InvalidNumber("offset"))
        );
        assert_eq!(
            p(&format!("u response {ID} {t} f 1 5\r")),
            Err(LegacyError::ControlByte)
        );
        assert_eq!(
            p(&format!("u response {ID} {t} f 1\t5")),
            Err(LegacyError::ControlByte)
        );
        assert_eq!(parse_line(b"u r i \xff"), Err(LegacyError::NotUtf8));
        assert!(p(&format!("u response {ID} {t}  f 1 5")).is_err());
    }

    #[test]
    fn legacy_lenient_numbers() {
        let s = format!("u response {ID} 20250503114301 - 000123 5");
        let e = p(&s).unwrap();
        assert_eq!(e.offset, Some(123));
        assert_eq!(e.filename, None);
        let s = format!("u response {ID} 20250503114301 - - 5");
        let e = p(&s).unwrap();
        assert_eq!(e.offset, None);
    }
}
