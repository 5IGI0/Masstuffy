//! Streaming reader over concatenated WARC/1.1 records.

use std::io::{self, BufRead, Read};

use crate::error::{ParseError, ReadError};
use crate::limits::Limits;
use crate::parse::{RecordRef, scan_head};

const VERSION_LINE: &[u8] = b"WARC/1.1\r\n";
/// Largest body buffer reserved up front, whatever `Content-Length` claims.
const MAX_PREALLOC: u64 = 1 << 20;

/// Sequential reader over concatenated WARC/1.1 records.
///
/// Reuses one internal buffer: each returned [`RecordRef`] borrows it until the next call (a lending
/// API, hence not an `Iterator`). Memory is bounded by `Limits::max_header_bytes` for the head, and
/// the body buffer grows only as bytes actually arrive (never from the declared `Content-Length`),
/// up to `Limits::max_body_len`.
///
/// Garbage between records is an error, not skipped. After any error the reader is poisoned.
pub struct WarcReader<R> {
    inner: R,
    buf: Vec<u8>,
    limits: Limits,
    offset: u64,
    poisoned: bool,
}

impl<R: BufRead> WarcReader<R> {
    /// Reader with `Limits::DEFAULT`.
    pub fn new(inner: R) -> Self {
        Self::with_limits(inner, Limits::DEFAULT)
    }

    /// Reader with explicit caps.
    pub fn with_limits(inner: R, limits: Limits) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            limits,
            offset: 0,
            poisoned: false,
        }
    }

    /// Reads the next record.
    ///
    /// Returns `Ok(None)` only on a clean EOF at a record boundary.
    ///
    /// # Errors
    /// `Io` from the underlying reader; `Parse` for malformed or truncated records (a partial record
    /// at EOF is `Parse(Truncated)`); after any error, every later call returns `Poisoned`.
    /// `ErrorKind::Interrupted` from the underlying reader is retried, never reported.
    ///
    /// The reader checks line endings, the header count and the header-size cap line by line
    /// while reading, before the full head is validated. For an invalid record it may therefore
    /// report a different error than [`RecordRef::parse`] on the same bytes; both always reject it.
    pub fn next_record(&mut self) -> Result<Option<RecordRef<'_>>, ReadError> {
        if self.poisoned {
            return Err(ReadError::Poisoned);
        }
        self.buf.clear();
        match self.fill() {
            Ok(true) => {}
            Ok(false) => return Ok(None),
            Err(e) => {
                self.poisoned = true;
                return Err(e);
            }
        }
        let Some(new_offset) = u64::try_from(self.buf.len())
            .ok()
            .and_then(|n| self.offset.checked_add(n))
        else {
            self.poisoned = true;
            return Err(io::Error::other("stream offset overflow").into());
        };
        match RecordRef::parse(&self.buf, &self.limits) {
            Ok(r) => {
                self.offset = new_offset;
                Ok(Some(r))
            }
            Err(e) => {
                self.poisoned = true;
                Err(e.into())
            }
        }
    }

    /// Stream offset of the start of the next record (sum of the sizes of the records returned).
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Returns the underlying reader.
    pub fn into_inner(self) -> R {
        self.inner
    }

    #[cfg(test)]
    fn buf_capacity(&self) -> usize {
        self.buf.capacity()
    }

    /// Reads one full record into `self.buf`. `Ok(false)` on clean EOF before any byte.
    fn fill(&mut self) -> Result<bool, ReadError> {
        let at_eof = loop {
            match self.inner.fill_buf() {
                Ok(b) => break b.is_empty(),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        };
        if at_eof {
            return Ok(false);
        }
        let max = self.limits.max_header_bytes;
        let too_large = ParseError::HeaderSectionTooLarge { limit: max };
        let mut headers = 0usize;
        let mut first = true;
        loop {
            let start = self.buf.len();
            let budget = max.checked_sub(start).ok_or(too_large.clone())?;
            let budget = u64::try_from(budget).unwrap_or(u64::MAX);
            Read::take(&mut self.inner, budget).read_until(b'\n', &mut self.buf)?;
            let line = self.buf.get(start..).unwrap_or_default();
            if line.last() != Some(&b'\n') {
                if self.buf.len() >= max {
                    return Err(too_large.into());
                }
                if first && !VERSION_LINE.starts_with(&self.buf) {
                    return Err(ParseError::InvalidVersion.into());
                }
                return Err(ParseError::Truncated.into());
            }
            if first {
                if line != VERSION_LINE {
                    return Err(ParseError::InvalidVersion.into());
                }
                first = false;
                continue;
            }
            if !line.ends_with(b"\r\n") {
                return Err(ParseError::InvalidLineEnding {
                    offset: self.buf.len().saturating_sub(1),
                }
                .into());
            }
            if line.len() == 2 {
                break;
            }
            headers += 1;
            if headers > self.limits.max_headers {
                return Err(ParseError::TooManyHeaders {
                    limit: self.limits.max_headers,
                }
                .into());
            }
        }

        let content_length = scan_head(&self.buf, &self.limits)?.content_length;
        // Only fails for UNLIMITED caps and an absurd length that no stream can satisfy.
        let total = content_length.checked_add(4).ok_or(ParseError::Truncated)?;
        self.buf
            .reserve(usize::try_from(total.min(MAX_PREALLOC)).unwrap_or(0));
        let read = Read::take(&mut self.inner, total).read_to_end(&mut self.buf)?;
        if u64::try_from(read).ok() != Some(total) {
            return Err(ParseError::Truncated.into());
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{MIN, MIN_BODY, rec, same_view};
    use std::io::BufReader;

    fn three() -> Vec<Vec<u8>> {
        vec![
            rec(MIN, MIN_BODY),
            rec("WARC-Type: metadata\r\nContent-Length: 0\r\n", b""),
            rec(
                "WARC-Type: response\r\nX: y\r\nContent-Length: 8\r\n",
                b"a\r\n\r\nbcd",
            ),
        ]
    }

    fn check_all<R: BufRead>(mut r: WarcReader<R>, recs: &[Vec<u8>]) {
        let mut total = 0u64;
        for bytes in recs {
            let expected = RecordRef::parse(bytes, &Limits::DEFAULT).unwrap();
            let got = r.next_record().unwrap().unwrap();
            same_view(&got, &expected);
            total += u64::try_from(bytes.len()).unwrap();
            assert_eq!(r.offset(), total);
        }
        assert!(r.next_record().unwrap().is_none());
        assert!(r.next_record().unwrap().is_none());
        assert_eq!(r.offset(), total);
    }

    #[test]
    fn reads_concatenated() {
        let recs = three();
        let all = recs.concat();
        check_all(WarcReader::new(&all[..]), &recs);
    }

    #[test]
    fn small_bufreader_capacity() {
        let recs = three();
        let all = recs.concat();
        check_all(
            WarcReader::new(BufReader::with_capacity(1, &all[..])),
            &recs,
        );
    }

    #[test]
    fn empty_stream() {
        let mut r = WarcReader::new(&b""[..]);
        assert!(r.next_record().unwrap().is_none());
    }

    fn assert_truncated_then_poisoned(bytes: &[u8]) {
        let mut r = WarcReader::new(bytes);
        assert!(matches!(
            r.next_record(),
            Err(ReadError::Parse(ParseError::Truncated))
        ));
        assert!(matches!(r.next_record(), Err(ReadError::Poisoned)));
        assert!(matches!(r.next_record(), Err(ReadError::Poisoned)));
    }

    #[test]
    fn truncated_mid_head() {
        let b = rec(MIN, MIN_BODY);
        assert_truncated_then_poisoned(&b[..30]);
        assert_truncated_then_poisoned(&b[..5]);
    }

    #[test]
    fn truncated_mid_body() {
        let b = rec(MIN, MIN_BODY);
        assert_truncated_then_poisoned(&b[..b.len() - 6]);
    }

    #[test]
    fn truncated_in_trailer() {
        let b = rec(MIN, MIN_BODY);
        assert_truncated_then_poisoned(&b[..b.len() - 2]);
    }

    #[test]
    fn every_prefix_is_truncated() {
        let b = rec(MIN, MIN_BODY);
        for n in 1..b.len() {
            assert_truncated_then_poisoned(&b[..n]);
        }
    }

    #[test]
    fn garbage_between_records() {
        for garbage in [&b"garbage\r\n"[..], b"xx", b"\r\n"] {
            let mut all = rec(MIN, MIN_BODY);
            all.extend_from_slice(garbage);
            all.extend_from_slice(&rec(MIN, MIN_BODY));
            let mut r = WarcReader::new(&all[..]);
            assert!(r.next_record().unwrap().is_some());
            assert!(matches!(
                r.next_record(),
                Err(ReadError::Parse(ParseError::InvalidVersion))
            ));
            assert!(matches!(r.next_record(), Err(ReadError::Poisoned)));
        }
    }

    #[test]
    fn bare_lf_is_rejected() {
        let b = b"WARC/1.1\r\nWARC-Type: x\nContent-Length: 0\r\n\r\n\r\n\r\n";
        let mut r = WarcReader::new(&b[..]);
        assert!(matches!(
            r.next_record(),
            Err(ReadError::Parse(ParseError::InvalidLineEnding { .. }))
        ));
    }

    #[test]
    fn huge_declared_body_does_not_allocate() {
        let mut b = b"WARC/1.1\r\nWARC-Type: x\r\nContent-Length: 1073741824\r\n\r\n".to_vec();
        b.extend_from_slice(b"0123456789");
        let mut r = WarcReader::new(&b[..]);
        assert!(matches!(
            r.next_record(),
            Err(ReadError::Parse(ParseError::Truncated))
        ));
        assert!(r.buf_capacity() <= 2 << 20, "{}", r.buf_capacity());
    }

    #[test]
    fn over_limit_body() {
        let lim = Limits {
            max_body_len: 10,
            ..Limits::DEFAULT
        };
        let mut b = b"WARC/1.1\r\nWARC-Type: x\r\nContent-Length: 11\r\n\r\n".to_vec();
        b.extend_from_slice(b"01234567890\r\n\r\n");
        let mut r = WarcReader::with_limits(&b[..], lim);
        assert!(matches!(
            r.next_record(),
            Err(ReadError::Parse(ParseError::BodyTooLarge {
                length: 11,
                limit: 10
            }))
        ));
        assert!(r.buf_capacity() < 1024);
    }

    #[test]
    fn endless_header_line() {
        let mut r = WarcReader::new(BufReader::new(io::repeat(b'a')));
        assert!(matches!(
            r.next_record(),
            Err(ReadError::Parse(ParseError::HeaderSectionTooLarge {
                limit: 65536
            }))
        ));
        let prefix = b"WARC/1.1\r\n".chain(io::repeat(b'a'));
        let mut r = WarcReader::new(BufReader::new(prefix));
        assert!(matches!(
            r.next_record(),
            Err(ReadError::Parse(ParseError::HeaderSectionTooLarge { .. }))
        ));
    }

    #[test]
    fn endless_header_lines() {
        let prefix = b"WARC/1.1\r\n".chain(io::repeat(b'\n'));
        let mut r = WarcReader::new(BufReader::new(prefix));
        assert!(matches!(
            r.next_record(),
            Err(ReadError::Parse(ParseError::InvalidLineEnding { .. }))
        ));
        let mut stream = b"WARC/1.1\r\n".to_vec();
        for _ in 0..300 {
            stream.extend_from_slice(b"X: 1\r\n");
        }
        let mut r = WarcReader::new(&stream[..]);
        assert!(matches!(
            r.next_record(),
            Err(ReadError::Parse(ParseError::TooManyHeaders { limit: 256 }))
        ));
    }

    struct Flaky<'a> {
        data: &'a [u8],
    }

    impl Read for Flaky<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.data.is_empty() {
                return Err(io::Error::other("boom"));
            }
            let n = out.len().min(self.data.len());
            let (head, tail) = self.data.split_at(n);
            out.get_mut(..n).unwrap().copy_from_slice(head);
            self.data = tail;
            Ok(n)
        }
    }

    #[test]
    fn io_error_propagates() {
        let b = rec(MIN, MIN_BODY);
        for n in [0usize, 5, 40, b.len() - 3] {
            let mut r = WarcReader::new(BufReader::new(Flaky { data: &b[..n] }));
            assert!(matches!(r.next_record(), Err(ReadError::Io(_))), "n={n}");
            assert!(matches!(r.next_record(), Err(ReadError::Poisoned)));
        }
    }

    /// Returns `Interrupted` on the first read, then serves `data`.
    struct InterruptOnce<'a> {
        data: &'a [u8],
        fired: bool,
    }

    impl Read for InterruptOnce<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if !self.fired {
                self.fired = true;
                return Err(io::ErrorKind::Interrupted.into());
            }
            self.data.read(out)
        }
    }

    #[test]
    fn interrupted_is_retried() {
        let b = rec(MIN, MIN_BODY);
        let mut r = WarcReader::new(BufReader::new(InterruptOnce {
            data: &b,
            fired: false,
        }));
        assert_eq!(r.next_record().unwrap().unwrap().as_bytes(), &b[..]);
        assert!(r.next_record().unwrap().is_none());
    }

    #[test]
    fn into_inner_and_send() {
        fn check<T: Send>() {}
        check::<WarcReader<&[u8]>>();
        let r = WarcReader::new(&b""[..]);
        assert!(r.into_inner().is_empty());
    }
}
