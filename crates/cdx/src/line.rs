//! Encoding and parsing of header and entry lines.

use std::borrow::Cow;
use std::fmt;

use masstuffy_warc::WarcDate;

use crate::entry::{Entry, Kind};
use crate::error::{EntryError, HeaderError, LineError};
use crate::escape::{escape_url, unescape_url};

/// Longest header line, `\n` included.
pub(crate) const MAX_HEADER_LINE: usize = 64;
/// Prefix shared by every header version.
pub(crate) const HEADER_PREFIX: &[u8] = b"#masstuffy-cdx/";

fn put(out: &mut Vec<u8>, args: fmt::Arguments<'_>) {
    // Writing into a Vec<u8> cannot fail.
    let _ = std::io::Write::write_fmt(out, args);
}

/// Low 32 bits of the xxh3-64 of the line content.
fn line_checksum(content: &[u8]) -> u32 {
    let h = xxhash_rust::xxh3::xxh3_64(content);
    let [a, b, c, d, ..] = h.to_le_bytes();
    u32::from_le_bytes([a, b, c, d])
}

/// Canonical decimal: 1 to 20 digits, no leading zero unless exactly `0`, fits `u64`.
fn parse_canonical_u64(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.is_empty() || (b.len() > 1 && b.first() == Some(&b'0')) {
        return None;
    }
    b.iter().try_fold(0u64, |n, &d| {
        if !d.is_ascii_digit() {
            return None;
        }
        n.checked_mul(10)?.checked_add(u64::from(d - b'0'))
    })
}

/// Exactly `len` lowercase hex digits (`len <= 16`), as a number.
fn parse_hex(s: &[u8], len: usize) -> Option<u64> {
    if s.len() != len {
        return None;
    }
    s.iter().try_fold(0u64, |n, &d| {
        let v = match d {
            b'0'..=b'9' => d - b'0',
            b'a'..=b'f' => d - b'a' + 10,
            _ => return None,
        };
        Some((n << 4) | u64::from(v))
    })
}

/// Appends `#masstuffy-cdx/1 gen=<gen>\n`.
pub fn encode_header(generation: u32, out: &mut Vec<u8>) {
    put(out, format_args!("#masstuffy-cdx/1 gen={generation}\n"));
}

/// Parses one header line **without** its `\n`. Returns `gen`.
///
/// # Errors
/// `NotCdx` if the prefix is wrong, `UnsupportedVersion` for a well-formed other version, `Invalid`
/// if the rest does not match ` gen=<canonical u32>`.
pub fn parse_header(line: &[u8]) -> Result<u32, HeaderError> {
    let rest = line
        .strip_prefix(HEADER_PREFIX)
        .ok_or(HeaderError::NotCdx)?;
    let digits_len = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    let (ver, rest) = rest.split_at(digits_len);
    let ver = std::str::from_utf8(ver).map_err(|_| HeaderError::Invalid)?;
    if ver.is_empty() || ver.len() > 9 {
        return Err(HeaderError::Invalid);
    }
    let version = parse_canonical_u64(ver)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or(HeaderError::Invalid)?;
    if version != 1 {
        return Err(HeaderError::UnsupportedVersion { version });
    }
    let generation = rest
        .strip_prefix(b" gen=")
        .and_then(|g| std::str::from_utf8(g).ok())
        .and_then(parse_canonical_u64)
        .and_then(|g| u32::try_from(g).ok())
        .ok_or(HeaderError::Invalid)?;
    Ok(generation)
}

/// Validates `entry`, appends one complete line (with checksum and `\n`) to `out`.
///
/// On error `out` is left exactly as it was.
///
/// # Errors
/// Any [`Entry::validate`] error, or `LineTooLong { len, limit }` if the line, `\n` included,
/// exceeds `max_line`.
pub fn encode_entry(
    entry: &Entry<'_>,
    max_line: usize,
    out: &mut Vec<u8>,
) -> Result<(), EntryError> {
    entry.validate()?;
    let start = out.len();
    let kind = match entry.kind {
        Kind::Add => 'A',
        Kind::Tombstone => 'T',
    };
    put(
        out,
        format_args!(
            "{kind} {} {} {} {:016x} {} {} {} ",
            entry.segment,
            entry.offset,
            entry.raw_size,
            entry.digest,
            entry.date,
            entry.warc_type,
            entry.record_id
        ),
    );
    match &entry.url {
        None => out.push(b'-'),
        Some(u) => escape_url(u, out),
    }
    let check = out.get(start..).map_or(0, line_checksum);
    put(out, format_args!(" {check:08x}\n"));
    let len = out.len() - start;
    if len > max_line {
        out.truncate(start);
        return Err(EntryError::LineTooLong {
            len,
            limit: max_line,
        });
    }
    Ok(())
}

fn number<'a>(
    it: &mut impl Iterator<Item = &'a str>,
    name: &'static str,
) -> Result<u64, LineError> {
    let f = it.next().ok_or(LineError::FieldCount)?;
    parse_canonical_u64(f).ok_or(LineError::InvalidNumber(name))
}

/// Parses one entry line **without** its `\n`.
///
/// Order: checksum (fast reject), UTF-8 of the content, split into exactly 9 fields, each field.
/// A returned entry satisfies [`Entry::validate`] and re-encodes to the same bytes.
///
/// # Errors
/// See [`LineError`]; `Checksum` for a missing, malformed or wrong check field.
pub fn parse_line(line: &[u8]) -> Result<Entry<'_>, LineError> {
    let split = line.len().checked_sub(9).ok_or(LineError::Checksum)?;
    let (content, tail) = line.split_at(split);
    let (sp, check) = tail.split_first().ok_or(LineError::Checksum)?;
    if *sp != b' ' {
        return Err(LineError::Checksum);
    }
    let check = parse_hex(check, 8).ok_or(LineError::Checksum)?;
    if check != u64::from(line_checksum(content)) {
        return Err(LineError::Checksum);
    }
    let content = std::str::from_utf8(content).map_err(|_| LineError::NotUtf8)?;
    let mut it = content.split(' ');
    let kind = match it.next().ok_or(LineError::FieldCount)? {
        "A" => Kind::Add,
        "T" => Kind::Tombstone,
        _ => return Err(LineError::InvalidKind),
    };
    let segment = u32::try_from(number(&mut it, "segment")?)
        .map_err(|_| LineError::InvalidNumber("segment"))?;
    let offset = number(&mut it, "offset")?;
    let raw_size = number(&mut it, "raw_size")?;
    let digest = it.next().ok_or(LineError::FieldCount)?;
    let digest = parse_hex(digest.as_bytes(), 16).ok_or(LineError::InvalidDigest)?;
    let date = it.next().ok_or(LineError::FieldCount)?;
    // `Display` trims trailing zeros of the fraction: reject other spellings to keep one encoding.
    if date.contains('.') && date.ends_with("0Z") {
        return Err(LineError::InvalidDate);
    }
    let date = WarcDate::parse(date).map_err(|_| LineError::InvalidDate)?;
    let warc_type = it.next().ok_or(LineError::FieldCount)?;
    let record_id = it.next().ok_or(LineError::FieldCount)?;
    let url = unescape_url(it.next().ok_or(LineError::FieldCount)?)?;
    if it.next().is_some() {
        return Err(LineError::FieldCount);
    }
    let entry = Entry {
        kind,
        segment,
        offset,
        raw_size,
        digest,
        date,
        warc_type: Cow::Borrowed(warc_type),
        record_id: Cow::Borrowed(record_id),
        url,
    };
    entry.validate()?;
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{sample, with_check};

    fn hdr(s: &str) -> Result<u32, HeaderError> {
        parse_header(s.as_bytes())
    }

    #[test]
    fn header_roundtrip() {
        for generation in [0, 1, u32::MAX] {
            let mut v = Vec::new();
            encode_header(generation, &mut v);
            assert_eq!(v.pop(), Some(b'\n'));
            assert_eq!(parse_header(&v), Ok(generation));
        }
    }

    #[test]
    fn header_rejects() {
        assert_eq!(hdr(""), Err(HeaderError::NotCdx));
        assert_eq!(hdr("garbage"), Err(HeaderError::NotCdx));
        assert_eq!(hdr("#other"), Err(HeaderError::NotCdx));
        for h in [
            "#masstuffy-cdx/",
            "#masstuffy-cdx/1",
            "#masstuffy-cdx/1 gen=",
            "#masstuffy-cdx/1 gen=01",
            "#masstuffy-cdx/1 gen=4294967296",
            "#masstuffy-cdx/1 gen=1 ",
            "#masstuffy-cdx/1 gen=1 x=2",
            "#masstuffy-cdx/1 gen=-1",
            "#masstuffy-cdx/01 gen=1",
        ] {
            assert_eq!(hdr(h), Err(HeaderError::Invalid), "{h}");
        }
        for h in ["#masstuffy-cdx/2 gen=1", "#masstuffy-cdx/2 whatever"] {
            assert_eq!(hdr(h), Err(HeaderError::UnsupportedVersion { version: 2 }));
        }
    }

    fn enc(e: &Entry<'_>) -> Vec<u8> {
        let mut v = Vec::new();
        encode_entry(e, usize::MAX, &mut v).unwrap();
        v
    }

    #[test]
    fn entry_roundtrip() {
        let mut list = Vec::new();
        for i in 0..6 {
            list.push(sample(i));
        }
        let mut e = sample(0);
        e.kind = Kind::Tombstone;
        list.push(e.clone());
        for url in [
            None,
            Some("-"),
            Some("a b"),
            Some("a\nb"),
            Some("%"),
            Some("é ü"),
        ] {
            let mut x = e.clone();
            x.kind = Kind::Add;
            x.url = url.map(|u| Cow::Owned(u.to_owned()));
            list.push(x);
        }
        let mut x = e.clone();
        x.segment = u32::MAX;
        x.offset = u64::MAX - 1;
        x.raw_size = 1;
        x.digest = u64::MAX;
        list.push(x.clone());
        x.digest = 0;
        x.date = WarcDate::new(2025, 1, 2, 3, 4, 5, 123_456_789).unwrap();
        list.push(x.clone());
        x.date = WarcDate::new(2025, 1, 2, 3, 4, 5, 500_000_000).unwrap();
        list.push(x);
        for e in list {
            let bytes = enc(&e);
            let body = bytes.strip_suffix(b"\n").unwrap();
            let back = parse_line(body).unwrap();
            assert_eq!(back, e);
            assert_eq!(enc(&back), bytes);
        }
    }

    #[test]
    fn entry_encode_rejects_and_leaves_buffer() {
        let base = sample(1);
        let cases: Vec<(Entry<'static>, EntryError)> = vec![
            (
                Entry {
                    raw_size: 0,
                    ..base.clone()
                },
                EntryError::ZeroRawSize,
            ),
            (
                Entry {
                    offset: u64::MAX,
                    raw_size: 1,
                    ..base.clone()
                },
                EntryError::LocatorOverflow,
            ),
            (
                Entry {
                    warc_type: "".into(),
                    ..base.clone()
                },
                EntryError::Empty("warc_type"),
            ),
            (
                Entry {
                    record_id: "a b".into(),
                    ..base.clone()
                },
                EntryError::InvalidAtom("record_id"),
            ),
            (
                Entry {
                    url: Some("".into()),
                    ..base.clone()
                },
                EntryError::Empty("url"),
            ),
        ];
        for (e, err) in cases {
            let mut out = b"xyz".to_vec();
            assert_eq!(encode_entry(&e, 1000, &mut out), Err(err));
            assert_eq!(out, b"xyz");
        }
        let e = Entry {
            url: Some("u".repeat(300).into()),
            ..base
        };
        let mut out = b"xyz".to_vec();
        assert!(matches!(
            encode_entry(&e, 200, &mut out),
            Err(EntryError::LineTooLong { limit: 200, .. })
        ));
        assert_eq!(out, b"xyz");
    }

    #[test]
    fn entry_line_len_exact_cap() {
        let e = sample(2);
        let len = enc(&e).len();
        let mut out = Vec::new();
        assert!(encode_entry(&e, len, &mut out).is_ok());
        let mut out = Vec::new();
        assert!(matches!(
            encode_entry(&e, len - 1, &mut out),
            Err(EntryError::LineTooLong { .. })
        ));
    }

    #[test]
    fn parse_rejects_checksum() {
        let e = sample(3);
        let bytes = enc(&e);
        let line = bytes.strip_suffix(b"\n").unwrap();
        for i in 0..line.len() {
            let mut l = line.to_vec();
            l[i] ^= 0x01;
            match parse_line(&l) {
                Err(_) => {}
                Ok(x) => panic!("accepted flipped byte {i}: {x:?}"),
            }
            if i < line.len() - 9 {
                assert_eq!(parse_line(&l).unwrap_err(), LineError::Checksum);
            }
        }
        let mut upper = line.to_vec();
        let n = upper.len();
        for b in &mut upper[n - 8..] {
            *b = b.to_ascii_uppercase();
        }
        if upper != line {
            assert_eq!(parse_line(&upper), Err(LineError::Checksum));
        }
        assert_eq!(parse_line(b"short"), Err(LineError::Checksum));
        assert_eq!(parse_line(b""), Err(LineError::Checksum));
    }

    const ID: &str = "urn:uuid:00000000-0000-4000-8000-000000000000";
    const DG: &str = "0123456789abcdef";
    const DT: &str = "2025-05-03T11:43:01Z";

    fn f(content: &str) -> LineError {
        let line = with_check(content);
        parse_line(line.strip_suffix(b"\n").unwrap()).unwrap_err()
    }

    #[test]
    fn parse_rejects_with_valid_checksum() {
        let ok = format!("A 1 2 3 {DG} {DT} response {ID} http://x/");
        assert!(parse_line(with_check(&ok).strip_suffix(b"\n").unwrap()).is_ok());
        assert_eq!(
            f(&format!("A 1 2 3 {DG} {DT} response {ID}")),
            LineError::FieldCount
        );
        assert_eq!(f(&format!("{ok} extra")), LineError::FieldCount);
        let e = f(&format!("A 1 2 3 {DG} {DT}  {ID} http://x/"));
        assert!(matches!(
            e,
            LineError::Entry(EntryError::Empty("warc_type"))
        ));
        assert_eq!(
            f(&format!("A  1 2 3 {DG} {DT} response {ID} u")),
            LineError::InvalidNumber("segment")
        );
        assert_eq!(
            f(&format!("X 1 2 3 {DG} {DT} response {ID} u")),
            LineError::InvalidKind
        );
        for off in ["01", "-1", "123456789012345678901", "18446744073709551616"] {
            assert_eq!(
                f(&format!("A 1 {off} 3 {DG} {DT} response {ID} u")),
                LineError::InvalidNumber("offset")
            );
        }
        assert_eq!(
            f(&format!("A 4294967296 2 3 {DG} {DT} response {ID} u")),
            LineError::InvalidNumber("segment")
        );
        for dg in ["0123456789abcde", "0123456789ABCDEF", "0123456789abcdeg"] {
            assert_eq!(
                f(&format!("A 1 2 3 {dg} {DT} response {ID} u")),
                LineError::InvalidDigest
            );
        }
        assert_eq!(
            f(&format!(
                "A 1 2 3 {DG} 2025-02-30T00:00:00Z response {ID} u"
            )),
            LineError::InvalidDate
        );
        assert_eq!(
            f(&format!(
                "A 1 2 3 {DG} 2025-05-03T11:43:01.50Z response {ID} u"
            )),
            LineError::InvalidDate
        );
        assert_eq!(
            f(&format!("A 1 2 0 {DG} {DT} response {ID} u")),
            LineError::Entry(EntryError::ZeroRawSize)
        );
        assert_eq!(
            f(&format!(
                "A 1 18446744073709551615 2 {DG} {DT} response {ID} u"
            )),
            LineError::Entry(EntryError::LocatorOverflow)
        );
        assert_eq!(
            f(&format!("A 1 2 3 {DG} {DT} response {ID} %41")),
            LineError::InvalidEscape
        );
        for u in ["http://a\rb", "http://a\x01%20", "a\x7f"] {
            assert_eq!(
                f(&format!("A 1 2 3 {DG} {DT} response {ID} {u}")),
                LineError::InvalidEscape
            );
        }
        let mut raw = format!("A 1 2 3 {DG} {DT} response {ID} u").into_bytes();
        raw.push(0xFF);
        let mut line = raw.clone();
        let check = line_checksum(&raw);
        line.extend_from_slice(format!(" {check:08x}").as_bytes());
        assert_eq!(parse_line(&line), Err(LineError::NotUtf8));
    }

    #[test]
    fn digest_is_xxh3() {
        for d in [&b""[..], b"hello"] {
            assert_eq!(crate::digest(d), xxhash_rust::xxh3::xxh3_64(d));
        }
    }
}
