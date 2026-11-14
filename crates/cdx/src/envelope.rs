//! Streaming reader of the raw-import envelope: a CDX line followed by `raw_size` bytes, repeated.

use std::borrow::Cow;
use std::io::{BufRead, Read};

use masstuffy_warc::WarcDate;

use crate::DEFAULT_MAX_LINE;
use crate::entry::Kind;
use crate::error::EnvelopeError;
use crate::legacy;
use crate::line::{HEADER_PREFIX, MAX_HEADER_LINE, parse_header, parse_line};

/// Most bytes reserved for a payload before they have arrived.
const MAX_PREALLOC: u64 = 1 << 20;

/// Limits of an [`EnvelopeReader`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvelopeLimits {
    /// Longest line, `\n` included.
    pub max_line: usize,
    /// Largest accepted `raw_size`, checked before any payload byte is read.
    pub max_record: u64,
}

impl EnvelopeLimits {
    /// 256 KiB lines, 1 GiB records.
    pub const DEFAULT: Self = Self {
        max_line: DEFAULT_MAX_LINE,
        max_record: 1 << 30,
    };
}

/// Line format of an envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeFormat {
    /// `#masstuffy-cdx/1` header, then entry lines.
    V1,
    /// Legacy lines (spec 3.4), no header.
    Legacy,
}

/// What the client claims about one raw record. Not trusted: the collection checks the frame and
/// compares `digest` (if present) with its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportMeta<'a> {
    /// `WARC-Type`.
    pub warc_type: Cow<'a, str>,
    /// `WARC-Record-ID` without `<>`.
    pub record_id: Cow<'a, str>,
    /// `WARC-Target-URI`, `None` if absent.
    pub url: Option<Cow<'a, str>>,
    /// `WARC-Date`.
    pub date: WarcDate,
    /// Size of the payload that follows; at least 1 and at most `max_record`.
    pub raw_size: u64,
    /// Claimed xxh3-64 digest (V1 only).
    pub digest: Option<u64>,
}

impl ImportMeta<'_> {
    /// Owned copy (text fields become `Cow::Owned`).
    #[must_use]
    pub fn into_owned(self) -> ImportMeta<'static> {
        ImportMeta {
            warc_type: Cow::Owned(self.warc_type.into_owned()),
            record_id: Cow::Owned(self.record_id.into_owned()),
            url: self.url.map(|u| Cow::Owned(u.into_owned())),
            date: self.date,
            raw_size: self.raw_size,
            digest: self.digest,
        }
    }
}

/// One record of the envelope.
#[derive(Debug)]
pub struct EnvelopeItem<'a> {
    /// The metadata, borrowing the reader's line buffer.
    pub meta: ImportMeta<'a>,
    /// Exactly `meta.raw_size` bytes.
    pub payload: Vec<u8>,
}

/// Reads an envelope incrementally. Poisoned after any error.
#[derive(Debug)]
pub struct EnvelopeReader<R> {
    inner: R,
    line: Vec<u8>,
    first_pending: bool,
    format: EnvelopeFormat,
    limits: EnvelopeLimits,
    offset: u64,
    done: bool,
    poisoned: bool,
}

fn fail<T>(poisoned: &mut bool, e: EnvelopeError) -> Result<T, EnvelopeError> {
    *poisoned = true;
    Err(e)
}

fn limit_of(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

impl<R: BufRead> EnvelopeReader<R> {
    /// Reads the first line (bounded by `max_line`). An empty stream gives a reader that yields
    /// nothing. A first line starting with `#masstuffy-cdx/` is a header (`gen` is ignored): format
    /// `V1`. Anything else is format `Legacy` and that line is the first record line.
    ///
    /// # Errors
    /// `InvalidMaxLine` if `limits.max_line < 64`, `Header` for a bad header (including an
    /// unsupported version), `LineTooLong`, `Truncated` (header without newline), `Io`.
    pub fn new(mut inner: R, limits: EnvelopeLimits) -> Result<Self, EnvelopeError> {
        if limits.max_line < MAX_HEADER_LINE {
            return Err(EnvelopeError::InvalidMaxLine(limits.max_line));
        }
        let mut line = Vec::new();
        let n = Read::take(&mut inner, limit_of(limits.max_line)).read_until(b'\n', &mut line)?;
        let mut r = Self {
            inner,
            line,
            first_pending: false,
            format: EnvelopeFormat::Legacy,
            limits,
            offset: 0,
            done: n == 0,
            poisoned: false,
        };
        if n == 0 {
            return Ok(r);
        }
        if r.line.starts_with(HEADER_PREFIX) {
            let Some(body) = r.line.strip_suffix(b"\n") else {
                return Err(if n >= limits.max_line {
                    EnvelopeError::LineTooLong {
                        offset: 0,
                        limit: limits.max_line,
                    }
                } else {
                    EnvelopeError::Truncated { offset: 0 }
                });
            };
            parse_header(body)?;
            r.format = EnvelopeFormat::V1;
            r.offset = limit_of(n);
        } else {
            r.first_pending = true;
        }
        Ok(r)
    }

    /// The detected format.
    #[must_use]
    pub fn format(&self) -> EnvelopeFormat {
        self.format
    }

    /// Stream offset of the next line.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Next record. `Ok(None)` only at a clean EOF at a line boundary.
    ///
    /// `raw_size > max_record` is refused before any payload byte is read; the payload buffer
    /// reserves at most 1 MiB up front and grows as bytes arrive.
    ///
    /// # Errors
    /// See [`EnvelopeError`]; `Poisoned` after any earlier error.
    pub fn next_item(&mut self) -> Result<Option<EnvelopeItem<'_>>, EnvelopeError> {
        if self.poisoned {
            return Err(EnvelopeError::Poisoned);
        }
        if self.done {
            return Ok(None);
        }
        // The returned meta borrows `self.line`: below, only other fields are touched.
        let n = if self.first_pending {
            self.first_pending = false;
            self.line.len()
        } else {
            self.line.clear();
            match Read::take(&mut self.inner, limit_of(self.limits.max_line))
                .read_until(b'\n', &mut self.line)
            {
                Ok(n) => n,
                Err(e) => return fail(&mut self.poisoned, e.into()),
            }
        };
        let at = self.offset;
        if n == 0 {
            self.done = true;
            return Ok(None);
        }
        let Some(body) = self.line.strip_suffix(b"\n") else {
            let e = if n >= self.limits.max_line {
                EnvelopeError::LineTooLong {
                    offset: at,
                    limit: self.limits.max_line,
                }
            } else {
                EnvelopeError::Truncated { offset: at }
            };
            return fail(&mut self.poisoned, e);
        };
        let meta = match self.format {
            EnvelopeFormat::V1 => match parse_line(body) {
                Ok(e) if e.kind == Kind::Tombstone => {
                    return fail(&mut self.poisoned, EnvelopeError::Tombstone { offset: at });
                }
                Ok(e) => ImportMeta {
                    warc_type: e.warc_type,
                    record_id: e.record_id,
                    url: e.url,
                    date: e.date,
                    raw_size: e.raw_size,
                    digest: Some(e.digest),
                },
                Err(source) => {
                    return fail(
                        &mut self.poisoned,
                        EnvelopeError::Line { offset: at, source },
                    );
                }
            },
            EnvelopeFormat::Legacy => match legacy::parse_line(body) {
                Ok(e) => ImportMeta {
                    warc_type: Cow::Borrowed(e.warc_type),
                    record_id: Cow::Borrowed(e.record_id),
                    url: e.url.map(Cow::Borrowed),
                    date: e.date,
                    raw_size: e.raw_size,
                    digest: None,
                },
                Err(source) => {
                    return fail(
                        &mut self.poisoned,
                        EnvelopeError::Legacy { offset: at, source },
                    );
                }
            },
        };
        let size = meta.raw_size;
        if size > self.limits.max_record {
            return fail(
                &mut self.poisoned,
                EnvelopeError::RecordTooLarge {
                    offset: at,
                    size,
                    limit: self.limits.max_record,
                },
            );
        }
        let cap = usize::try_from(size.min(MAX_PREALLOC)).unwrap_or(0);
        let mut payload = Vec::with_capacity(cap);
        match Read::take(&mut self.inner, size).read_to_end(&mut payload) {
            Ok(got) if limit_of(got) == size => {}
            Ok(_) => return fail(&mut self.poisoned, EnvelopeError::Truncated { offset: at }),
            Err(e) => return fail(&mut self.poisoned, e.into()),
        }
        let next = at
            .checked_add(limit_of(n))
            .and_then(|v| v.checked_add(size));
        let Some(next) = next else {
            return fail(&mut self.poisoned, EnvelopeError::OffsetOverflow);
        };
        self.offset = next;
        Ok(Some(EnvelopeItem { meta, payload }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HeaderError;
    use crate::entry::Entry;
    use crate::line::{encode_entry, encode_header};
    use crate::testutil::{sample, with_check};
    use std::io::{self, Cursor};

    const ID: &str = "urn:uuid:0f3e5a1c-0000-4000-8000-000000000000";

    fn v1(items: &[(Entry<'static>, Vec<u8>)]) -> Vec<u8> {
        let mut v = Vec::new();
        encode_header(0, &mut v);
        for (e, p) in items {
            let mut e = e.clone();
            e.segment = 0;
            e.offset = 0;
            e.raw_size = p.len() as u64;
            encode_entry(&e, usize::MAX, &mut v).unwrap();
            v.extend_from_slice(p);
        }
        v
    }

    #[test]
    fn import_meta_into_owned() {
        let items = vec![(sample(1), b"payload".to_vec())];
        let data = v1(&items);
        let (expected, owned) = {
            let mut r = EnvelopeReader::new(&data[..], EnvelopeLimits::DEFAULT).unwrap();
            let it = r.next_item().unwrap().unwrap();
            let expected = (
                it.meta.record_id.to_string(),
                it.meta.url.as_deref().map(str::to_owned),
                it.meta.date,
            );
            (expected, it.meta.into_owned())
        };
        assert_eq!(owned.record_id, expected.0);
        assert_eq!(owned.url.as_deref().map(str::to_owned), expected.1);
        assert_eq!(owned.date, expected.2);
        assert_eq!(owned.record_id, items[0].0.record_id);
    }

    #[test]
    fn envelope_v1_roundtrip() {
        let items: Vec<_> = (0..3u32)
            .map(|i| {
                (
                    sample(i),
                    vec![b'a' + u8::try_from(i).unwrap(); 10 + usize::try_from(i).unwrap()],
                )
            })
            .collect();
        let data = v1(&items);
        let mut r = EnvelopeReader::new(&data[..], EnvelopeLimits::DEFAULT).unwrap();
        assert_eq!(r.format(), EnvelopeFormat::V1);
        for (e, p) in &items {
            let it = r.next_item().unwrap().unwrap();
            assert_eq!(it.meta.warc_type, e.warc_type);
            assert_eq!(it.meta.record_id, e.record_id);
            assert_eq!(it.meta.url, e.url);
            assert_eq!(it.meta.date, e.date);
            assert_eq!(it.meta.digest, Some(e.digest));
            assert_eq!(it.meta.raw_size, p.len() as u64);
            assert_eq!(&it.payload, p);
        }
        assert_eq!(r.offset(), data.len() as u64);
        assert!(r.next_item().unwrap().is_none());
        assert!(r.next_item().unwrap().is_none());
    }

    #[test]
    fn envelope_legacy_roundtrip() {
        let mut data = Vec::new();
        let urls = ["http://a/b c d", "http://x/", "-"];
        for (i, u) in urls.iter().enumerate() {
            data.extend_from_slice(
                format!("{u} response {ID} 20250503114301 f {i} {}\n", 3 + i).as_bytes(),
            );
            data.extend_from_slice(&vec![b'p'; 3 + i]);
        }
        let mut r = EnvelopeReader::new(&data[..], EnvelopeLimits::DEFAULT).unwrap();
        assert_eq!(r.format(), EnvelopeFormat::Legacy);
        for (i, u) in urls.iter().enumerate() {
            let it = r.next_item().unwrap().unwrap();
            assert_eq!(it.meta.url.as_deref(), (*u != "-").then_some(*u));
            assert_eq!(it.meta.digest, None);
            assert_eq!(it.payload.len(), 3 + i);
        }
        assert!(r.next_item().unwrap().is_none());
    }

    #[test]
    fn envelope_empty() {
        let mut r = EnvelopeReader::new(&b""[..], EnvelopeLimits::DEFAULT).unwrap();
        assert!(r.next_item().unwrap().is_none());
    }

    struct Counting<'a> {
        inner: Cursor<&'a [u8]>,
        read: usize,
    }
    impl io::Read for Counting<'_> {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(b)?;
            self.read += n;
            Ok(n)
        }
    }
    impl BufRead for Counting<'_> {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            self.inner.fill_buf()
        }
        fn consume(&mut self, n: usize) {
            self.read += n;
            self.inner.consume(n);
        }
    }

    #[test]
    fn envelope_size_cap() {
        let line = format!("u response {ID} 20250503114301 f 1 11\n");
        let mut data = line.clone().into_bytes();
        data.extend_from_slice(&[b'x'; 11]);
        let limits = EnvelopeLimits {
            max_line: 1000,
            max_record: 10,
        };
        let mut r = EnvelopeReader::new(
            Counting {
                inner: Cursor::new(&data),
                read: 0,
            },
            limits,
        )
        .unwrap();
        assert!(matches!(
            r.next_item(),
            Err(EnvelopeError::RecordTooLarge {
                size: 11,
                limit: 10,
                ..
            })
        ));
        assert!(matches!(r.next_item(), Err(EnvelopeError::Poisoned)));
        assert_eq!(r.inner.read, line.len());
    }

    #[test]
    fn envelope_huge_claim_no_alloc() {
        let data = format!("u response {ID} 20250503114301 f 1 18446744073709551000\nabcde");
        let limits = EnvelopeLimits {
            max_line: 1000,
            max_record: u64::MAX,
        };
        let mut r = EnvelopeReader::new(data.as_bytes(), limits).unwrap();
        assert!(matches!(
            r.next_item(),
            Err(EnvelopeError::Truncated { .. })
        ));
    }

    #[test]
    fn envelope_truncated() {
        let d = format!("u response {ID} 20250503114301 f 1 5\nabc");
        let mut r = EnvelopeReader::new(d.as_bytes(), EnvelopeLimits::DEFAULT).unwrap();
        assert!(matches!(
            r.next_item(),
            Err(EnvelopeError::Truncated { offset: 0 })
        ));
        let d = format!("u response {ID} 20250503114301 f 1 5");
        let mut r = EnvelopeReader::new(d.as_bytes(), EnvelopeLimits::DEFAULT).unwrap();
        assert!(matches!(
            r.next_item(),
            Err(EnvelopeError::Truncated { offset: 0 })
        ));
    }

    #[test]
    fn envelope_line_too_long() {
        let d = format!(
            "{} response {ID} 20250503114301 f 1 5\nabcde",
            "u".repeat(200)
        );
        let limits = EnvelopeLimits {
            max_line: 100,
            max_record: 100,
        };
        let mut r = EnvelopeReader::new(d.as_bytes(), limits).unwrap();
        assert!(matches!(
            r.next_item(),
            Err(EnvelopeError::LineTooLong { limit: 100, .. })
        ));
    }

    #[test]
    fn envelope_rejects_tombstone_and_zero() {
        let mut t = sample(1);
        t.kind = Kind::Tombstone;
        let mut data = Vec::new();
        encode_header(0, &mut data);
        encode_entry(&t, usize::MAX, &mut data).unwrap();
        data.extend_from_slice(&[0u8; 200]);
        let mut r = EnvelopeReader::new(&data[..], EnvelopeLimits::DEFAULT).unwrap();
        assert!(matches!(
            r.next_item(),
            Err(EnvelopeError::Tombstone { .. })
        ));

        let mut data = Vec::new();
        encode_header(0, &mut data);
        data.extend_from_slice(&with_check(&format!(
            "A 0 0 0 0123456789abcdef 2025-05-03T11:43:01Z response {ID} u"
        )));
        let mut r = EnvelopeReader::new(&data[..], EnvelopeLimits::DEFAULT).unwrap();
        assert!(matches!(
            r.next_item(),
            Err(EnvelopeError::Line {
                source: crate::LineError::Entry(crate::EntryError::ZeroRawSize),
                ..
            })
        ));

        let d = format!("u response {ID} 20250503114301 f 1 0\n");
        let mut r = EnvelopeReader::new(d.as_bytes(), EnvelopeLimits::DEFAULT).unwrap();
        assert!(matches!(
            r.next_item(),
            Err(EnvelopeError::Legacy {
                source: legacy::LegacyError::ZeroRawSize,
                ..
            })
        ));
    }

    #[test]
    fn envelope_bad_header() {
        let r = EnvelopeReader::new(&b"#masstuffy-cdx/9 gen=0\n"[..], EnvelopeLimits::DEFAULT);
        assert!(matches!(
            r,
            Err(EnvelopeError::Header(HeaderError::UnsupportedVersion {
                version: 9
            }))
        ));
    }

    #[test]
    fn rejects_small_max_line() {
        for max_line in [0, 63] {
            let limits = EnvelopeLimits {
                max_line,
                ..EnvelopeLimits::DEFAULT
            };
            assert!(matches!(
                EnvelopeReader::new(&b"x\n"[..], limits),
                Err(EnvelopeError::InvalidMaxLine(n)) if n == max_line
            ));
        }
    }
}
