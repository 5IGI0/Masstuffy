//! Owned records and the builder that produces them.

use crate::date::WarcDate;
use crate::error::BuildError;
use crate::header::{
    CONTENT_LENGTH, WARC_DATE, WARC_RECORD_ID, WARC_TARGET_URI, WARC_TYPE, parse_record_id,
    validate_name, validate_record_id_inner, validate_value,
};
use crate::limits::Limits;
use crate::parse::RecordRef;

const VERSION_LINE: &[u8] = b"WARC/1.1\r\n";

/// Owned serialized record.
///
/// Invariant: `bytes` is a record accepted by `RecordRef::parse(&bytes, &Limits::UNLIMITED)`. It is
/// only ever produced by [`RecordBuilder::build`] or copied from a [`RecordRef`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    bytes: Vec<u8>,
}

impl Record {
    /// The serialized record.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the record and returns its serialized bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Re-parses the bytes (O(head size), no allocation). Infallible by the type invariant.
    pub fn view(&self) -> RecordRef<'_> {
        #[allow(
            clippy::expect_used,
            reason = "Record invariant: bytes come from RecordBuilder::build (which uses the parser's \
                      validators) or from a RecordRef; see test `build_output_always_parses`"
        )]
        RecordRef::parse(&self.bytes, &Limits::UNLIMITED)
            .expect("Record invariant: bytes built from validated parts")
    }
}

impl From<RecordRef<'_>> for Record {
    /// Copies the raw bytes exactly (byte-faithful round trip).
    fn from(r: RecordRef<'_>) -> Self {
        Record {
            bytes: r.as_bytes().to_vec(),
        }
    }
}

impl AsRef<[u8]> for Record {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

/// Builds a new record.
///
/// Header order is insertion order. `Content-Length` is always computed by [`build`](Self::build)
/// and written as the last header.
#[derive(Clone, Debug, Default)]
pub struct RecordBuilder {
    headers: Vec<(String, String)>,
}

fn is_unique(name: &str) -> bool {
    [WARC_TYPE, WARC_RECORD_ID, WARC_DATE]
        .iter()
        .any(|u| name.eq_ignore_ascii_case(u))
}

fn push_decimal(out: &mut Vec<u8>, n: usize) {
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    let mut n = n;
    loop {
        i -= 1; // at most 20 digits for a u64, so i never underflows
        if let Some(slot) = digits.get_mut(i) {
            *slot = b'0' + u8::try_from(n % 10).unwrap_or(0);
        }
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(digits.get(i..).unwrap_or_default());
}

impl RecordBuilder {
    /// Starts with `WARC-Type: <warc_type>`.
    ///
    /// # Errors
    /// `InvalidHeaderValue` if `warc_type` is not a non-empty token.
    pub fn new(warc_type: &str) -> Result<Self, BuildError> {
        let mut b = Self::default();
        b.header(WARC_TYPE, warc_type)?;
        Ok(b)
    }

    /// Copies every header of `r` except `Content-Length`, in order. Infallible: `r` is validated.
    pub fn from_record_ref(r: &RecordRef<'_>) -> Self {
        let mut headers = Vec::with_capacity(r.header_count());
        for h in r.headers() {
            if !h.name.eq_ignore_ascii_case(CONTENT_LENGTH) {
                headers.push((h.name.to_owned(), h.value.to_owned()));
            }
        }
        Self { headers }
    }

    /// Adds a header after validating name and value with the parser's rules.
    ///
    /// For `WARC-Type`, `WARC-Record-ID` and `WARC-Date` an existing entry is replaced in place
    /// (spec: a second occurrence replaces the first); other names are appended, so they may repeat.
    ///
    /// # Errors
    /// `InvalidHeaderName`; `ReservedHeader` for `Content-Length`; `InvalidRecordId` if
    /// `WARC-Record-ID` is not `<uri>`; `InvalidHeaderValue` for a bad value, a `WARC-Type` that is
    /// not a token or a `WARC-Date` that does not parse.
    pub fn header(&mut self, name: &str, value: &str) -> Result<&mut Self, BuildError> {
        if !validate_name(name.as_bytes()) {
            return Err(BuildError::InvalidHeaderName);
        }
        if name.eq_ignore_ascii_case(CONTENT_LENGTH) {
            return Err(BuildError::ReservedHeader);
        }
        let bad_value = || BuildError::InvalidHeaderValue {
            name: name.to_owned(),
        };
        if !validate_value(value.as_bytes()) {
            return Err(bad_value());
        }
        if name.eq_ignore_ascii_case(WARC_TYPE) {
            if !validate_name(value.as_bytes()) {
                return Err(bad_value());
            }
        } else if name.eq_ignore_ascii_case(WARC_RECORD_ID) {
            if parse_record_id(value).is_none() {
                return Err(BuildError::InvalidRecordId);
            }
        } else if name.eq_ignore_ascii_case(WARC_DATE) && WarcDate::parse(value).is_err() {
            return Err(bad_value());
        }
        self.set(name, value);
        Ok(self)
    }

    /// Inserts without validation; callers validated. Replaces in place for unique headers.
    fn set(&mut self, name: &str, value: &str) {
        if is_unique(name)
            && let Some(slot) = self
                .headers
                .iter_mut()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
        {
            value.clone_into(&mut slot.1);
            return;
        }
        self.headers.push((name.to_owned(), value.to_owned()));
    }

    /// Sets `WARC-Record-ID` to `<id>`; `id` is given without brackets.
    ///
    /// # Errors
    /// `InvalidRecordId` unless `id` is non-empty, contains `:` and has no space, control byte,
    /// `<` or `>`.
    pub fn record_id(&mut self, id: &str) -> Result<&mut Self, BuildError> {
        if !validate_record_id_inner(id) {
            return Err(BuildError::InvalidRecordId);
        }
        self.set(WARC_RECORD_ID, &format!("<{id}>"));
        Ok(self)
    }

    /// Sets `WARC-Date`.
    pub fn date(&mut self, date: WarcDate) -> &mut Self {
        self.set(WARC_DATE, &date.to_string());
        self
    }

    /// Appends a `WARC-Target-URI` header (no URI validation beyond the generic value rules).
    ///
    /// # Errors
    /// `InvalidHeaderValue` if `uri` contains a control byte.
    pub fn target_uri(&mut self, uri: &str) -> Result<&mut Self, BuildError> {
        self.header(WARC_TARGET_URI, uri)
    }

    /// First value for `name` (ASCII case-insensitive).
    pub fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Fills in what is absent: `WARC-Record-ID` becomes `<urn:uuid:{v4}>` and `WARC-Date` the
    /// current time truncated to whole microseconds. Present values are kept.
    ///
    /// # Errors
    /// `ClockOutOfRange` if the system clock is outside 1970..=9999.
    #[cfg(feature = "generate")]
    pub fn fill_missing(&mut self) -> Result<&mut Self, BuildError> {
        if self.get(WARC_RECORD_ID).is_none() {
            let id = uuid::Uuid::new_v4();
            self.set(WARC_RECORD_ID, &format!("<{}>", id.urn()));
        }
        if self.get(WARC_DATE).is_none() {
            let now = WarcDate::from_system_time(std::time::SystemTime::now())
                .ok_or(BuildError::ClockOutOfRange)?;
            let micros = now.nanos() - now.nanos() % 1000;
            let date = WarcDate::new(
                now.year(),
                now.month(),
                now.day(),
                now.hour(),
                now.minute(),
                now.second(),
                micros,
            )
            .ok_or(BuildError::ClockOutOfRange)?;
            self.date(date);
        }
        Ok(self)
    }

    /// Serializes the record with `body`. One allocation of the exact output size.
    ///
    /// # Errors
    /// `MissingHeader` if `WARC-Type`, `WARC-Record-ID` or `WARC-Date` is absent; `TooLarge` if the
    /// size overflows.
    pub fn build(&self, body: &[u8]) -> Result<Record, BuildError> {
        for required in [WARC_TYPE, WARC_RECORD_ID, WARC_DATE] {
            if self.get(required).is_none() {
                return Err(BuildError::MissingHeader(required));
            }
        }
        let cl_digits = body.len().checked_ilog10().map_or(1, |l| {
            usize::try_from(l).map_or(usize::MAX, |l| l.saturating_add(1))
        });
        // "Content-Length: " + digits + CRLF
        let mut cap = CONTENT_LENGTH
            .len()
            .checked_add(2)
            .and_then(|n| n.checked_add(cl_digits))
            .and_then(|n| n.checked_add(2))
            .ok_or(BuildError::TooLarge)?;
        cap = cap
            .checked_add(VERSION_LINE.len() + 2) // version line + empty line
            .ok_or(BuildError::TooLarge)?;
        for (n, v) in &self.headers {
            cap = n
                .len()
                .checked_add(v.len())
                .and_then(|l| l.checked_add(4)) // ": " + CRLF
                .and_then(|l| cap.checked_add(l))
                .ok_or(BuildError::TooLarge)?;
        }
        cap = cap
            .checked_add(body.len())
            .and_then(|n| n.checked_add(4))
            .ok_or(BuildError::TooLarge)?;

        let mut out = Vec::with_capacity(cap);
        out.extend_from_slice(VERSION_LINE);
        for (n, v) in &self.headers {
            out.extend_from_slice(n.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(v.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(CONTENT_LENGTH.as_bytes());
        out.extend_from_slice(b": ");
        push_decimal(&mut out, body.len());
        out.extend_from_slice(b"\r\n\r\n");
        out.extend_from_slice(body);
        out.extend_from_slice(b"\r\n\r\n");
        Ok(Record { bytes: out })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MIN, MIN_BODY, rec, same_view};

    fn builder() -> RecordBuilder {
        let mut b = RecordBuilder::new("resource").unwrap();
        b.record_id("urn:x:1")
            .unwrap()
            .date(WarcDate::parse("2026-11-12T10:00:00Z").unwrap());
        b
    }

    #[test]
    fn byte_faithful_roundtrip() {
        let dup = rec(
            "X-A: 1\r\nContent-Length: 5\r\nx-a:  2 \r\nWARC-Type: resource\r\n",
            MIN_BODY,
        );
        for input in [rec(MIN, MIN_BODY), dup] {
            let view = RecordRef::parse(&input, &Limits::DEFAULT).unwrap();
            let r = Record::from(view);
            assert_eq!(r.as_bytes(), &input[..]);
            same_view(&r.view(), &view);
            assert_eq!(r.as_ref(), &input[..]);
            assert_eq!(r.into_bytes(), input);
        }
    }

    #[test]
    fn builder_basic() {
        let mut b = builder();
        b.target_uri("http://a/")
            .unwrap()
            .header("X-A", "1")
            .unwrap()
            .header("X-A", "2")
            .unwrap();
        let r = b.build(b"hello").unwrap();
        let expected = "WARC/1.1\r\nWARC-Type: resource\r\nWARC-Record-ID: <urn:x:1>\r\n\
            WARC-Date: 2026-11-12T10:00:00Z\r\nWARC-Target-URI: http://a/\r\nX-A: 1\r\nX-A: 2\r\n\
            Content-Length: 5\r\n\r\nhello\r\n\r\n";
        assert_eq!(r.as_bytes(), expected.as_bytes());
        assert_eq!(r.as_bytes().len(), r.clone().into_bytes().capacity());
        assert_eq!(b.get("x-a"), Some("1"));
        let v = r.view();
        assert_eq!(v.record_id(), Some("urn:x:1"));
        assert_eq!(v.target_uri(), Some("http://a/"));
    }

    #[test]
    fn builder_capacity_is_exact() {
        for len in [0usize, 1, 9, 10, 99, 100, 12345] {
            let body = vec![b'x'; len];
            let r = builder().build(&body).unwrap().into_bytes();
            assert_eq!(r.capacity(), r.len(), "len {len}");
            assert!(RecordRef::parse(&r, &Limits::DEFAULT).is_ok());
        }
    }

    #[test]
    fn builder_replaces_unique() {
        let mut b = builder();
        b.header("X-A", "1").unwrap();
        b.date(WarcDate::parse("2027-01-01T00:00:00Z").unwrap());
        b.header("x-a", "2").unwrap();
        b.header("warc-type", "metadata").unwrap();
        let r = b.build(b"").unwrap();
        let got: Vec<_> = r.view().headers().map(|h| (h.name, h.value)).collect();
        assert_eq!(
            got,
            [
                ("WARC-Type", "metadata"),
                ("WARC-Record-ID", "<urn:x:1>"),
                ("WARC-Date", "2027-01-01T00:00:00Z"),
                ("X-A", "1"),
                ("x-a", "2"),
                ("Content-Length", "0"),
            ]
        );
    }

    #[test]
    fn builder_rejects() {
        let mut b = builder();
        assert_eq!(
            b.header("Content-Length", "1").unwrap_err(),
            BuildError::ReservedHeader
        );
        assert_eq!(
            b.header("content-length", "1").unwrap_err(),
            BuildError::ReservedHeader
        );
        for name in ["", "a b", "a:b", "é"] {
            assert_eq!(
                b.header(name, "v").unwrap_err(),
                BuildError::InvalidHeaderName,
                "{name:?}"
            );
        }
        for v in ["a\r\nb: c", "a\x00b", "a\nb"] {
            assert!(matches!(
                b.header("X", v).unwrap_err(),
                BuildError::InvalidHeaderValue { .. }
            ));
        }
        assert_eq!(b.record_id("a b").unwrap_err(), BuildError::InvalidRecordId);
        assert_eq!(b.record_id("").unwrap_err(), BuildError::InvalidRecordId);
        assert_eq!(
            b.header("WARC-Record-ID", "urn:x:1").unwrap_err(),
            BuildError::InvalidRecordId
        );
        assert!(matches!(
            b.header("WARC-Date", "yesterday").unwrap_err(),
            BuildError::InvalidHeaderValue { .. }
        ));
        assert!(matches!(
            RecordBuilder::new("a b").unwrap_err(),
            BuildError::InvalidHeaderValue { .. }
        ));
        assert_eq!(
            RecordBuilder::new("resource")
                .unwrap()
                .build(b"")
                .unwrap_err(),
            BuildError::MissingHeader("WARC-Record-ID")
        );
        let mut only_id = RecordBuilder::new("resource").unwrap();
        only_id.record_id("a:b").unwrap();
        assert_eq!(
            only_id.build(b"").unwrap_err(),
            BuildError::MissingHeader("WARC-Date")
        );
        assert_eq!(
            RecordBuilder::default().build(b"").unwrap_err(),
            BuildError::MissingHeader("WARC-Type")
        );
    }

    #[test]
    fn from_record_ref_then_build() {
        let input = rec(MIN, MIN_BODY);
        let view = RecordRef::parse(&input, &Limits::DEFAULT).unwrap();
        let out = RecordBuilder::from_record_ref(&view)
            .build(MIN_BODY)
            .unwrap();
        assert_eq!(out.as_bytes(), &input[..]);

        let cl_first = rec(
            "Content-Length: 5\r\nWARC-Type: resource\r\nWARC-Record-ID: <urn:uuid:00000000-0000-4000-8000-000000000000>\r\nWARC-Date: 2026-11-12T10:00:00Z\r\n",
            MIN_BODY,
        );
        let view = RecordRef::parse(&cl_first, &Limits::DEFAULT).unwrap();
        let out = RecordBuilder::from_record_ref(&view)
            .build(MIN_BODY)
            .unwrap();
        assert_eq!(out.as_bytes(), &input[..]);
    }

    #[test]
    fn build_output_always_parses() {
        let long = "é".repeat(1000);
        let mut many = builder();
        for i in 0..200 {
            many.header(&format!("X-{i}"), &i.to_string()).unwrap();
        }
        let mut utf8 = builder();
        utf8.header("X-U", "héllo wörld ✓").unwrap();
        let mut empty_val = builder();
        empty_val.header("X-E", "").unwrap();
        let mut colon = builder();
        colon.header("X-C", " a: b: ").unwrap();
        let mut tab = builder();
        tab.header("X-T", "\ta\t").unwrap();
        let mut longv = builder();
        longv.header("X-L", &long).unwrap();
        let mut uri = builder();
        uri.target_uri("http://example.org/a b?c=d").unwrap();
        let crlf_body: &[u8] = b"a\r\n\r\nWARC/1.1\r\n\r\n";
        let cases: Vec<(RecordBuilder, &[u8])> = vec![
            (builder(), b""),
            (builder(), b"x"),
            (builder(), crlf_body),
            (many, b"body"),
            (utf8, b"\xff\x00binary"),
            (empty_val, b""),
            (colon, b"c"),
            (tab, b"t"),
            (longv, b"l"),
            (uri, b"u"),
        ];
        for (b, body) in cases {
            let r = b.build(body).unwrap();
            let parsed = RecordRef::parse(r.as_bytes(), &Limits::UNLIMITED).unwrap();
            same_view(&parsed, &r.view());
            assert_eq!(parsed.body(), body);
            assert_eq!(parsed.content_length(), u64::try_from(body.len()).unwrap());
        }
    }

    #[cfg(feature = "generate")]
    #[test]
    fn fill_missing_generates() {
        let mut b = RecordBuilder::new("resource").unwrap();
        b.fill_missing().unwrap();
        let r = b.build(b"").unwrap();
        let v = r.view();
        let id = v.record_id().unwrap();
        let hex = id.strip_prefix("urn:uuid:").unwrap();
        assert_eq!(hex.len(), 36);
        assert_eq!(hex.as_bytes().get(14), Some(&b'4'));
        assert!(v.date().unwrap().nanos().is_multiple_of(1000));

        let mut b2 = RecordBuilder::new("resource").unwrap();
        b2.fill_missing().unwrap();
        assert_ne!(b2.get(WARC_RECORD_ID), b.get(WARC_RECORD_ID));

        let mut kept = builder();
        kept.fill_missing().unwrap();
        assert_eq!(kept.get(WARC_RECORD_ID), Some("<urn:x:1>"));
        assert_eq!(kept.get(WARC_DATE), Some("2026-11-12T10:00:00Z"));
    }

    #[test]
    fn types_are_send_sync() {
        fn check<T: Send + Sync>() {}
        check::<Record>();
        check::<RecordRef<'static>>();
        check::<RecordBuilder>();
        check::<WarcDate>();
        check::<Limits>();
    }
}
