//! Zero-copy parsing of one WARC/1.1 record.

use crate::date::WarcDate;
use crate::error::ParseError;
use crate::header::{
    CONTENT_LENGTH, Headers, WARC_DATE, WARC_RECORD_ID, WARC_TARGET_URI, WARC_TYPE,
    first_invalid_value_byte, parse_content_length, parse_record_id, validate_name,
};
use crate::limits::Limits;

const VERSION_LINE: &[u8] = b"WARC/1.1\r\n";
const TRAILER: &[u8] = b"\r\n\r\n";

/// Result of scanning the head (version line, header lines, empty line) of a record.
pub(crate) struct Head<'a> {
    /// Bytes from the record start through the empty line, inclusive.
    pub(crate) head_len: usize,
    pub(crate) header_block: &'a str,
    pub(crate) warc_type: &'a str,
    pub(crate) record_id: Option<&'a str>,
    pub(crate) date: Option<WarcDate>,
    pub(crate) target_uri: Option<&'a str>,
    pub(crate) content_length: u64,
    pub(crate) header_count: usize,
}

/// Validates the version line and every header line (§ wire format rules), in one linear pass.
///
/// Errors are reported in input order. `Truncated` is returned only when the input is a proper
/// prefix of a head that would otherwise be valid under `limits`.
pub(crate) fn scan_head<'a>(input: &'a [u8], limits: &Limits) -> Result<Head<'a>, ParseError> {
    let vlen = VERSION_LINE.len();
    match input.get(..vlen) {
        Some(v) if v == VERSION_LINE => {}
        Some(_) => return Err(ParseError::InvalidVersion),
        None if VERSION_LINE.starts_with(input) => return Err(ParseError::Truncated),
        None => return Err(ParseError::InvalidVersion),
    }

    // Lines are only searched inside the cap, so an endless line is rejected as soon as it exceeds it.
    let window_end = input.len().min(limits.max_header_bytes);
    let mut pos = vlen;
    let mut count = 0usize;
    let mut warc_type = None;
    let mut record_id = None;
    let mut date = None;
    let mut target_uri = None;
    let mut content_length = None;

    let (head_len, block_end) = loop {
        let window = input.get(pos..window_end).unwrap_or_default();
        let Some(idx) = window.iter().position(|&b| b == b'\n') else {
            return Err(if input.len() > limits.max_header_bytes {
                ParseError::HeaderSectionTooLarge {
                    limit: limits.max_header_bytes,
                }
            } else {
                ParseError::Truncated
            });
        };
        let nl = pos + idx; // idx < window.len(), so this stays within the input
        let line = window
            .get(..idx)
            .and_then(|l| l.strip_suffix(b"\r"))
            .ok_or(ParseError::InvalidLineEnding { offset: nl })?;
        if line.is_empty() {
            break (nl + 1, pos);
        }

        if count >= limits.max_headers {
            return Err(ParseError::TooManyHeaders {
                limit: limits.max_headers,
            });
        }
        count += 1;

        let sep = line
            .windows(2)
            .position(|w| w == b": ")
            .ok_or(ParseError::MalformedHeaderLine { offset: pos })?;
        let (name_bytes, rest) = line.split_at(sep); // sep < line.len()
        let value_bytes = rest.get(2..).unwrap_or_default();
        let value_start = pos + sep + 2;
        if !validate_name(name_bytes) {
            return Err(ParseError::InvalidHeaderName { offset: pos });
        }
        if let Some(i) = first_invalid_value_byte(value_bytes) {
            return Err(ParseError::InvalidHeaderValue {
                offset: value_start + i,
            });
        }
        let value =
            std::str::from_utf8(value_bytes).map_err(|e| ParseError::InvalidHeaderValue {
                offset: value_start + e.valid_up_to(),
            })?;
        // The name is a token, hence ASCII.
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| ParseError::InvalidHeaderName { offset: pos })?;

        if name.eq_ignore_ascii_case(WARC_TYPE) {
            if warc_type.is_some() {
                return Err(ParseError::DuplicateHeader(WARC_TYPE));
            }
            if !validate_name(value_bytes) {
                return Err(ParseError::InvalidHeaderValue {
                    offset: value_start,
                });
            }
            warc_type = Some(value);
        } else if name.eq_ignore_ascii_case(WARC_RECORD_ID) {
            if record_id.is_some() {
                return Err(ParseError::DuplicateHeader(WARC_RECORD_ID));
            }
            record_id = Some(parse_record_id(value).ok_or(ParseError::InvalidRecordId)?);
        } else if name.eq_ignore_ascii_case(WARC_DATE) {
            if date.is_some() {
                return Err(ParseError::DuplicateHeader(WARC_DATE));
            }
            date = Some(WarcDate::parse(value)?);
        } else if name.eq_ignore_ascii_case(CONTENT_LENGTH) {
            if content_length.is_some() {
                return Err(ParseError::DuplicateHeader(CONTENT_LENGTH));
            }
            let length = parse_content_length(value_bytes)?;
            if length > limits.max_body_len {
                return Err(ParseError::BodyTooLarge {
                    length,
                    limit: limits.max_body_len,
                });
            }
            content_length = Some(length);
        } else if name.eq_ignore_ascii_case(WARC_TARGET_URI) && target_uri.is_none() {
            target_uri = Some(value);
        }
        pos = nl + 1;
    };

    let warc_type = warc_type.ok_or(ParseError::MissingHeader(WARC_TYPE))?;
    let content_length = content_length.ok_or(ParseError::MissingHeader(CONTENT_LENGTH))?;
    // Every line was validated above (names are ASCII, values UTF-8, separators ASCII), so this
    // cannot fail; it only produces the `&str` view without unsafe code.
    let header_block = input
        .get(vlen..block_end)
        .map(std::str::from_utf8)
        .transpose()
        .map_err(|e| ParseError::InvalidHeaderValue {
            offset: vlen + e.valid_up_to(),
        })?
        .unwrap_or_default();
    Ok(Head {
        head_len,
        header_block,
        warc_type,
        record_id,
        date,
        target_uri,
        content_length,
        header_count: count,
    })
}

/// Zero-copy view of one validated WARC/1.1 record.
///
/// Invariants: `raw` is exactly one complete record (head + body + `\r\n\r\n`) satisfying the wire
/// rules; `header_block` is the UTF-8 text between the version line and the empty line (each line
/// including its CRLF); the body length equals `Content-Length`; the unique headers (`WARC-Type`,
/// `WARC-Record-ID`, `WARC-Date`, `Content-Length`) appear at most once.
#[derive(Clone, Copy, Debug)]
pub struct RecordRef<'a> {
    raw: &'a [u8],
    header_block: &'a str,
    body: &'a [u8],
    warc_type: &'a str,
    record_id: Option<&'a str>,
    date: Option<WarcDate>,
    target_uri: Option<&'a str>,
    content_length: u64,
    header_count: usize,
}

impl<'a> RecordRef<'a> {
    /// Parses exactly one record that must span all of `input`.
    ///
    /// # Errors
    /// Any `ParseError`; bytes after the record give `TrailingData`. Never panics, never allocates.
    pub fn parse(input: &'a [u8], limits: &Limits) -> Result<Self, ParseError> {
        let (rec, used) = Self::parse_prefix(input, limits)?;
        match input.len().checked_sub(used) {
            Some(0) => Ok(rec),
            Some(extra) => Err(ParseError::TrailingData { extra }),
            None => Err(ParseError::Truncated), // used <= input.len() by construction
        }
    }

    /// Parses one record at the start of `input`; returns it and the number of bytes consumed.
    ///
    /// # Errors
    /// Any `ParseError` except `TrailingData`. A proper prefix of a valid record gives `Truncated`.
    pub fn parse_prefix(input: &'a [u8], limits: &Limits) -> Result<(Self, usize), ParseError> {
        let head = scan_head(input, limits)?;
        let len = usize::try_from(head.content_length).map_err(|_| ParseError::Truncated)?;
        let body_end = head
            .head_len
            .checked_add(len)
            .ok_or(ParseError::Truncated)?;
        let trailer_end = body_end
            .checked_add(TRAILER.len())
            .ok_or(ParseError::Truncated)?;
        if input.len() < body_end {
            return Err(ParseError::Truncated);
        }
        match input.get(body_end..trailer_end) {
            Some(t) if t == TRAILER => {}
            Some(_) => return Err(ParseError::InvalidTrailer),
            None => {
                let have = input.get(body_end..).unwrap_or_default();
                return Err(if TRAILER.starts_with(have) {
                    ParseError::Truncated
                } else {
                    ParseError::InvalidTrailer
                });
            }
        }
        let (raw, _) = input
            .split_at_checked(trailer_end)
            .ok_or(ParseError::Truncated)?;
        let body = raw
            .get(head.head_len..body_end)
            .ok_or(ParseError::Truncated)?;
        Ok((
            RecordRef {
                raw,
                header_block: head.header_block,
                body,
                warc_type: head.warc_type,
                record_id: head.record_id,
                date: head.date,
                target_uri: head.target_uri,
                content_length: head.content_length,
                header_count: head.header_count,
            },
            trailer_end,
        ))
    }

    /// Value of `WARC-Type`, a non-empty token.
    pub fn warc_type(&self) -> &'a str {
        self.warc_type
    }

    /// `WARC-Record-ID` without the surrounding `<>`, if present.
    pub fn record_id(&self) -> Option<&'a str> {
        self.record_id
    }

    /// `WARC-Date`, if present.
    pub fn date(&self) -> Option<WarcDate> {
        self.date
    }

    /// First `WARC-Target-URI`, verbatim, if present.
    pub fn target_uri(&self) -> Option<&'a str> {
        self.target_uri
    }

    /// Declared (and verified) body length.
    pub fn content_length(&self) -> u64 {
        self.content_length
    }

    /// The record body.
    pub fn body(&self) -> &'a [u8] {
        self.body
    }

    /// The exact input bytes of the record (byte-faithful).
    pub fn as_bytes(&self) -> &'a [u8] {
        self.raw
    }

    /// First header with this name (ASCII case-insensitive), verbatim value. O(head size).
    pub fn header(&self, name: &str) -> Option<&'a str> {
        self.headers()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value)
    }

    /// All headers in input order, `Content-Length` included.
    pub fn headers(&self) -> Headers<'a> {
        Headers::new(self.header_block)
    }

    /// Number of header lines.
    pub fn header_count(&self) -> usize {
        self.header_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MIN, MIN_BODY, rec, same_view};

    const D: Limits = Limits::DEFAULT;

    fn min() -> Vec<u8> {
        rec(MIN, MIN_BODY)
    }

    fn parse(b: &[u8]) -> Result<RecordRef<'_>, ParseError> {
        RecordRef::parse(b, &D)
    }

    fn err(b: &[u8]) -> ParseError {
        parse(b).unwrap_err()
    }

    /// MIN with an extra header line inserted before `Content-Length`.
    fn with_extra(extra: &[u8]) -> Vec<u8> {
        let mut v = b"WARC/1.1\r\nWARC-Type: resource\r\n".to_vec();
        v.extend_from_slice(extra);
        v.extend_from_slice(b"Content-Length: 5\r\n\r\nhello\r\n\r\n");
        v
    }

    #[test]
    fn parse_minimal() {
        let b = min();
        let r = parse(&b).unwrap();
        assert_eq!(r.warc_type(), "resource");
        assert_eq!(
            r.record_id(),
            Some("urn:uuid:00000000-0000-4000-8000-000000000000")
        );
        assert_eq!(r.date(), WarcDate::parse("2026-11-12T10:00:00Z").ok());
        assert_eq!(r.target_uri(), None);
        assert_eq!(r.content_length(), 5);
        assert_eq!(r.body(), b"hello");
        assert_eq!(r.as_bytes(), &b[..]);
        assert_eq!(r.header_count(), 4);
        assert_eq!(r.header("content-length"), Some("5"));
    }

    #[test]
    fn target_uri_is_first_occurrence() {
        let b = rec(
            &format!("WARC-Target-URI: http://a/ b\r\nwarc-target-uri: x\r\n{MIN}"),
            MIN_BODY,
        );
        assert_eq!(parse(&b).unwrap().target_uri(), Some("http://a/ b"));
    }

    #[test]
    fn headers_preserve_order_and_duplicates() {
        let b = rec(
            &format!(
                "X-A: 1\r\nWARC-Concurrent-To: <a:1>\r\nx-a: 2\r\nWARC-Concurrent-To: <a:2>\r\n{MIN}"
            ),
            MIN_BODY,
        );
        let r = parse(&b).unwrap();
        let got: Vec<_> = r.headers().map(|h| (h.name, h.value)).collect();
        assert_eq!(got[0], ("X-A", "1"));
        assert_eq!(got[1], ("WARC-Concurrent-To", "<a:1>"));
        assert_eq!(got[2], ("x-a", "2"));
        assert_eq!(got[3], ("WARC-Concurrent-To", "<a:2>"));
        assert_eq!(got[4].0, "WARC-Type");
        assert_eq!(got.len(), 8);
        assert_eq!(r.header_count(), 8);
        assert_eq!(r.header("X-A"), Some("1"));
        assert_eq!(r.header("nope"), None);
    }

    #[test]
    fn value_verbatim() {
        let b = with_extra("X: a: b \r\nY: \r\nZ: é\r\n".as_bytes());
        let r = parse(&b).unwrap();
        assert_eq!(r.header("X"), Some("a: b "));
        assert_eq!(r.header("Y"), Some(""));
        assert_eq!(r.header("Z"), Some("é"));
    }

    #[test]
    fn missing_id_and_date_allowed() {
        let b = rec("WARC-Type: resource\r\nContent-Length: 5\r\n", MIN_BODY);
        let r = parse(&b).unwrap();
        assert_eq!(r.record_id(), None);
        assert_eq!(r.date(), None);
    }

    #[test]
    fn empty_body() {
        let b = rec("WARC-Type: resource\r\nContent-Length: 0\r\n", b"");
        let r = parse(&b).unwrap();
        assert_eq!(r.body(), b"");
        assert_eq!(r.content_length(), 0);
    }

    #[test]
    fn body_with_crlfs() {
        let body = b"a\r\n\r\nWARC/1.1\r\nb\r\n\r\n";
        let b = rec(
            &format!("WARC-Type: resource\r\nContent-Length: {}\r\n", body.len()),
            body,
        );
        assert_eq!(parse(&b).unwrap().body(), body);
    }

    #[test]
    fn parse_prefix_concatenated() {
        let a = min();
        let c = rec("WARC-Type: metadata\r\nContent-Length: 0\r\n", b"");
        let mut both = a.clone();
        both.extend_from_slice(&c);
        let (r1, n1) = RecordRef::parse_prefix(&both, &D).unwrap();
        assert_eq!(n1, a.len());
        assert_eq!(r1.as_bytes(), &a[..]);
        let (r2, n2) = RecordRef::parse_prefix(&both[n1..], &D).unwrap();
        assert_eq!(n2, c.len());
        assert_eq!(r2.warc_type(), "metadata");
    }

    #[test]
    fn case_insensitive_known_headers() {
        let b = rec("warc-type: response\r\ncontent-length: 5\r\n", MIN_BODY);
        let r = parse(&b).unwrap();
        assert_eq!(r.warc_type(), "response");
        assert_eq!(r.content_length(), 5);
    }

    #[test]
    fn empty_input() {
        assert_eq!(err(b""), ParseError::Truncated);
    }

    #[test]
    fn every_prefix_is_truncated() {
        let b = min();
        for n in 0..b.len() {
            assert_eq!(
                RecordRef::parse(&b[..n], &D).unwrap_err(),
                ParseError::Truncated,
                "prefix {n}"
            );
            assert_eq!(
                RecordRef::parse_prefix(&b[..n], &D).unwrap_err(),
                ParseError::Truncated,
                "prefix {n}"
            );
        }
        assert!(RecordRef::parse(&b, &D).is_ok());
    }

    #[test]
    fn wrong_version() {
        for v in [
            "WARC/1.0\r\n",
            "warc/1.1\r\n",
            "WARC/1.1\n",
            "WARC/1.10\r\n",
            "WARC/2.0\r\n",
        ] {
            let mut b = v.as_bytes().to_vec();
            b.extend_from_slice(b"WARC-Type: x\r\nContent-Length: 0\r\n\r\n\r\n\r\n");
            assert_eq!(err(&b), ParseError::InvalidVersion, "{v:?}");
        }
        assert_eq!(err(b"WARC/1.0"), ParseError::InvalidVersion);
        assert_eq!(err(b"XARC"), ParseError::InvalidVersion);
    }

    #[test]
    fn bare_lf_in_header() {
        let b = with_extra(b"X: a\nb: c\r\n");
        assert!(matches!(err(&b), ParseError::InvalidLineEnding { .. }));
        let b = with_extra(b"\nX: a\r\n");
        assert!(matches!(err(&b), ParseError::InvalidLineEnding { .. }));
    }

    #[test]
    fn bare_cr_in_header() {
        let b = with_extra(b"X: a\rb\r\n");
        assert_eq!(err(&b), ParseError::InvalidHeaderValue { offset: 35 });
    }

    #[test]
    fn no_colon_space() {
        for l in ["X:1\r\n", "X 1\r\n", "X:\r\n"] {
            assert!(
                matches!(
                    err(&with_extra(l.as_bytes())),
                    ParseError::MalformedHeaderLine { .. }
                ),
                "{l:?}"
            );
        }
    }

    #[test]
    fn bad_names() {
        for l in [
            &b": v\r\n"[..],
            b"X Y: v\r\n",
            b" X: v\r\n",
            b"\tX: v\r\n",
            b"X\x00: v\r\n",
            "Xé: v\r\n".as_bytes(),
        ] {
            assert!(
                matches!(err(&with_extra(l)), ParseError::InvalidHeaderName { .. }),
                "{l:?}"
            );
        }
    }

    #[test]
    fn bad_values() {
        for l in [&b"X: a\x01b\r\n"[..], b"X: a\x7fb\r\n", b"X: \xff\r\n"] {
            assert!(
                matches!(err(&with_extra(l)), ParseError::InvalidHeaderValue { .. }),
                "{l:?}"
            );
        }
        // HT is allowed.
        assert!(parse(&with_extra(b"X: a\tb\r\n")).is_ok());
    }

    #[test]
    fn missing_required() {
        let b = rec("WARC-Record-ID: <a:b>\r\nContent-Length: 0\r\n", b"");
        assert_eq!(err(&b), ParseError::MissingHeader("WARC-Type"));
        let b = rec("WARC-Type: x\r\n", b"");
        assert_eq!(err(&b), ParseError::MissingHeader("Content-Length"));
        let b = rec("", b"");
        assert_eq!(err(&b), ParseError::MissingHeader("WARC-Type"));
    }

    #[test]
    fn duplicates_rejected() {
        let body = b"hello";
        for (extra, name) in [
            ("WARC-Type: x\r\n", "WARC-Type"),
            ("warc-type: x\r\n", "WARC-Type"),
            ("WARC-Record-ID: <a:b>\r\n", "WARC-Record-ID"),
            ("WARC-DATE: 2026-11-12T10:00:00Z\r\n", "WARC-Date"),
            ("content-length: 5\r\n", "Content-Length"),
        ] {
            let b = rec(&format!("{MIN}{extra}"), body);
            assert_eq!(err(&b), ParseError::DuplicateHeader(name), "{extra}");
        }
    }

    #[test]
    fn content_length_invalid() {
        for cl in ["-1", "05", "abc", "99999999999999999999", "", "+5"] {
            let b = rec(&format!("WARC-Type: x\r\nContent-Length: {cl}\r\n"), b"");
            assert_eq!(err(&b), ParseError::InvalidContentLength, "{cl:?}");
        }
    }

    #[test]
    fn content_length_over_limit() {
        let lim = Limits {
            max_body_len: 4,
            ..D
        };
        assert_eq!(
            RecordRef::parse(&min(), &lim).unwrap_err(),
            ParseError::BodyTooLarge {
                length: 5,
                limit: 4
            }
        );
        let b = rec(
            "WARC-Type: x\r\nContent-Length: 18446744073709551615\r\n",
            b"",
        );
        assert!(b.len() < 100);
        assert_eq!(
            err(&b),
            ParseError::BodyTooLarge {
                length: u64::MAX,
                limit: 1 << 30
            }
        );
        // Unlimited: huge length just means truncated, never an allocation or a panic.
        assert_eq!(
            RecordRef::parse(&b, &Limits::UNLIMITED).unwrap_err(),
            ParseError::Truncated
        );
    }

    #[test]
    fn body_shorter_than_declared() {
        let b = rec("WARC-Type: x\r\nContent-Length: 10\r\n", b"hello");
        assert_eq!(err(&b), ParseError::Truncated);
    }

    #[test]
    fn bad_trailer() {
        let head = &min()[..min().len() - 4];
        for tail in [&b"\r\n\rx"[..], b"\n\n\n\n", b"\r\n\r\x00"] {
            let mut b = head.to_vec();
            b.extend_from_slice(tail);
            assert_eq!(err(&b), ParseError::InvalidTrailer, "{tail:?}");
        }
        // partial wrong trailer is already invalid
        let mut b = head.to_vec();
        b.extend_from_slice(b"\n");
        assert_eq!(err(&b), ParseError::InvalidTrailer);
        // body longer than declared
        let b = rec("WARC-Type: x\r\nContent-Length: 5\r\n", b"hello!!");
        assert_eq!(err(&b), ParseError::InvalidTrailer);
    }

    #[test]
    fn trailing_data() {
        let mut b = min();
        let n = b.len();
        b.push(b'x');
        assert_eq!(err(&b), ParseError::TrailingData { extra: 1 });
        let (r, used) = RecordRef::parse_prefix(&b, &D).unwrap();
        assert_eq!(used, n);
        assert_eq!(r.as_bytes().len(), n);
    }

    #[test]
    fn header_section_cap() {
        let b = min();
        let head_len = b.len() - 5 - 4;
        let ok = Limits {
            max_header_bytes: head_len,
            ..D
        };
        assert!(RecordRef::parse(&b, &ok).is_ok());
        let small = Limits {
            max_header_bytes: head_len - 1,
            ..D
        };
        assert_eq!(
            RecordRef::parse(&b, &small).unwrap_err(),
            ParseError::HeaderSectionTooLarge {
                limit: head_len - 1
            }
        );
        let mut huge = b"WARC/1.1\r\n".to_vec();
        huge.resize(huge.len() + (1 << 20), b'a');
        assert_eq!(
            err(&huge),
            ParseError::HeaderSectionTooLarge { limit: 64 * 1024 }
        );
        let tiny = Limits {
            max_header_bytes: 3,
            ..D
        };
        assert_eq!(
            RecordRef::parse(&b, &tiny).unwrap_err(),
            ParseError::HeaderSectionTooLarge { limit: 3 }
        );
    }

    #[test]
    fn header_count_cap() {
        let lim = Limits {
            max_headers: 4,
            ..D
        };
        let b = rec(&format!("X: 1\r\n{MIN}"), MIN_BODY);
        assert_eq!(
            RecordRef::parse(&b, &lim).unwrap_err(),
            ParseError::TooManyHeaders { limit: 4 }
        );
        assert!(RecordRef::parse(&min(), &lim).is_ok());
    }

    #[test]
    fn bad_record_id() {
        for v in ["urn:x", "<>", "<a b:c>", "<noscheme>"] {
            let b = rec(
                &format!("WARC-Type: x\r\nWARC-Record-ID: {v}\r\nContent-Length: 0\r\n"),
                b"",
            );
            assert_eq!(err(&b), ParseError::InvalidRecordId, "{v}");
        }
    }

    #[test]
    fn bad_date() {
        let b = rec(
            "WARC-Type: x\r\nWARC-Date: yesterday\r\nContent-Length: 0\r\n",
            b"",
        );
        assert_eq!(err(&b), ParseError::InvalidDate);
    }

    #[test]
    fn bad_type() {
        for v in ["", "a b"] {
            let b = rec(&format!("WARC-Type: {v}\r\nContent-Length: 0\r\n"), b"");
            assert!(
                matches!(err(&b), ParseError::InvalidHeaderValue { .. }),
                "{v:?}"
            );
        }
    }

    struct XorShift(u64);
    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            usize::try_from(self.next() % u64::try_from(n).unwrap()).unwrap()
        }
        fn byte(&mut self) -> u8 {
            u8::try_from(self.next() & 0xFF).unwrap()
        }
    }

    #[test]
    fn garbage_never_panics() {
        let base = min();
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        for i in 0..10_000 {
            let input: Vec<u8> = if i % 4 == 0 {
                let n = rng.below(200);
                (0..n).map(|_| rng.byte()).collect()
            } else {
                let mut v = base.clone();
                match i % 4 {
                    1 => {
                        let p = rng.below(v.len());
                        v[p] = rng.byte();
                    }
                    2 => {
                        let p = rng.below(v.len() + 1);
                        v.insert(p, rng.byte());
                    }
                    _ => {
                        let p = rng.below(v.len());
                        if i % 8 == 3 {
                            v.remove(p);
                        } else {
                            v.truncate(p);
                        }
                    }
                }
                v
            };
            if let Ok(r) = RecordRef::parse(&input, &D) {
                let again = RecordRef::parse(r.as_bytes(), &D).unwrap();
                same_view(&r, &again);
            }
            if let Ok((r, n)) = RecordRef::parse_prefix(&input, &D) {
                assert_eq!(r.as_bytes().len(), n);
                let again = RecordRef::parse(r.as_bytes(), &D).unwrap();
                same_view(&r, &again);
            }
        }
    }
}
