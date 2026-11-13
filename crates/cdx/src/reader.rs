//! Streaming reader of index files with torn-tail detection.

use std::io::{self, BufRead, Read};

use crate::entry::Entry;
use crate::error::{HeaderError, LineError, ReadError};
use crate::line::{HEADER_PREFIX, MAX_HEADER_LINE, parse_header, parse_line};

/// One step of [`CdxReader::next_step`].
#[derive(Debug)]
pub enum Step<'a> {
    /// A valid line starting at byte `offset` of the stream.
    Entry {
        /// Stream offset of the line.
        offset: u64,
        /// The parsed entry, borrowing the reader's line buffer.
        entry: Entry<'a>,
    },
    /// End of the stream. Repeated on every later call.
    End(Tail),
}

/// How the stream ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tail {
    /// EOF at a line boundary; `end` is the stream length.
    Clean {
        /// Stream length.
        end: u64,
    },
    /// Invalid bytes from `at` to `end` (EOF) with no valid line after them: truncate to `at`.
    Torn {
        /// Offset of the first invalid line.
        at: u64,
        /// Stream length.
        end: u64,
    },
}

impl Tail {
    /// Length of the valid prefix: `end` if clean, `at` if torn.
    #[must_use]
    pub fn valid_len(&self) -> u64 {
        match *self {
            Self::Clean { end } => end,
            Self::Torn { at, .. } => at,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum State {
    Running,
    Ended(Tail),
    Poisoned,
}

/// Reads an index file one line at a time.
///
/// Classification of the first invalid line (see the crate docs): a line that is partial, too long,
/// or has a wrong checksum is a *torn candidate*; if no valid line follows it up to EOF the stream
/// ends with [`Tail::Torn`], otherwise it is [`ReadError::Corrupt`]. A line with a valid checksum
/// but invalid content is `Corrupt` at once. After an error the reader is poisoned.
///
/// `max_line` must be at least the writer's maximum line length (use
/// [`DEFAULT_MAX_LINE`](crate::DEFAULT_MAX_LINE)), otherwise valid long lines are reported as torn.
/// It must also be at least 64 (the header line cap).
#[derive(Debug)]
pub struct CdxReader<R> {
    inner: R,
    buf: Vec<u8>,
    generation: u32,
    offset: u64,
    max_line: usize,
    state: State,
}

fn check_max_line(max_line: usize) -> Result<(), ReadError> {
    if max_line < MAX_HEADER_LINE {
        return Err(ReadError::InvalidMaxLine(max_line));
    }
    Ok(())
}

fn limit_of(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// Skips to just after the next `\n` (or EOF) without buffering. Returns the bytes skipped.
fn skip_line<R: BufRead>(inner: &mut R) -> io::Result<u64> {
    let mut total = 0u64;
    loop {
        let (used, done) = match inner.fill_buf() {
            Ok([]) => return Ok(total),
            Ok(b) => match b.iter().position(|&c| c == b'\n') {
                Some(p) => (p + 1, true),
                None => (b.len(), false),
            },
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        inner.consume(used);
        total = total
            .checked_add(limit_of(used))
            .ok_or_else(|| io::Error::other("offset overflow"))?;
        if done {
            return Ok(total);
        }
    }
}

/// Scans the rest of the stream from `start` for a complete valid line.
/// Returns `(found, end_of_stream_offset_reached)`.
fn scan_rest<R: BufRead>(
    inner: &mut R,
    scratch: &mut Vec<u8>,
    max_line: usize,
    start: u64,
) -> io::Result<(bool, u64)> {
    let mut pos = start;
    let overflow = || io::Error::other("offset overflow");
    loop {
        scratch.clear();
        let n = Read::take(&mut *inner, limit_of(max_line)).read_until(b'\n', scratch)?;
        if n == 0 {
            return Ok((false, pos));
        }
        pos = pos.checked_add(limit_of(n)).ok_or_else(overflow)?;
        if let Some(body) = scratch.strip_suffix(b"\n") {
            if parse_line(body).is_ok() {
                return Ok((true, pos));
            }
        } else if n >= max_line {
            pos = pos.checked_add(skip_line(inner)?).ok_or_else(overflow)?;
        } else {
            // partial last line at EOF
            return Ok((false, pos));
        }
    }
}

impl<R: BufRead> CdxReader<R> {
    /// Reads and checks the header line (at most 64 bytes).
    ///
    /// # Errors
    /// `InvalidMaxLine` if `max_line < 64`, `MissingHeader` (EOF before a complete header line, including empty input), `Header` (also
    /// for an over-long line: `Invalid`, or `NotCdx` if the prefix already mismatches), `Io`.
    pub fn open(mut inner: R, max_line: usize) -> Result<Self, ReadError> {
        check_max_line(max_line)?;
        let mut buf = Vec::with_capacity(MAX_HEADER_LINE);
        let n = Read::take(&mut inner, limit_of(MAX_HEADER_LINE)).read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Err(ReadError::MissingHeader);
        }
        let Some(body) = buf.strip_suffix(b"\n") else {
            if n < MAX_HEADER_LINE {
                return Err(ReadError::MissingHeader);
            }
            let e = if buf.starts_with(HEADER_PREFIX) {
                HeaderError::Invalid
            } else {
                HeaderError::NotCdx
            };
            return Err(e.into());
        };
        let generation = parse_header(body)?;
        Ok(Self {
            inner,
            buf: Vec::new(),
            generation,
            offset: limit_of(n),
            max_line,
            state: State::Running,
        })
    }

    /// Continues reading at a line boundary the caller already validated (checkpoint replay):
    /// `inner` is positioned at stream offset `offset`, past the header. No header is read.
    ///
    /// # Errors
    /// `InvalidMaxLine` if `max_line < 64`.
    pub fn resume(
        inner: R,
        generation: u32,
        offset: u64,
        max_line: usize,
    ) -> Result<Self, ReadError> {
        check_max_line(max_line)?;
        Ok(Self {
            inner,
            buf: Vec::new(),
            generation,
            offset,
            max_line,
            state: State::Running,
        })
    }

    /// Generation number from the header.
    #[must_use]
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Offset of the next line (after the last line returned).
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Returns the underlying reader.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.inner
    }

    #[cfg(test)]
    fn buf_capacity(&self) -> usize {
        self.buf.capacity()
    }

    /// Next entry or the end of the stream. A lending API (the entry borrows the line buffer).
    ///
    /// # Errors
    /// `Io`; `Corrupt { at, source }` for corruption that is not a torn tail; `OffsetOverflow`;
    /// `Poisoned` after any earlier error.
    pub fn next_step(&mut self) -> Result<Step<'_>, ReadError> {
        match self.state {
            State::Ended(t) => return Ok(Step::End(t)),
            State::Poisoned => return Err(ReadError::Poisoned),
            State::Running => {}
        }
        // The returned entry borrows `self.buf` for the rest of this function, so below this point
        // only the other fields are touched (never `self.buf`).
        self.buf.clear();
        let n = match Read::take(&mut self.inner, limit_of(self.max_line))
            .read_until(b'\n', &mut self.buf)
        {
            Ok(n) => n,
            Err(e) => {
                self.state = State::Poisoned;
                return Err(e.into());
            }
        };
        let line_start = self.offset;
        if n == 0 {
            let t = Tail::Clean { end: line_start };
            self.state = State::Ended(t);
            return Ok(Step::End(t));
        }
        let Some(next) = line_start.checked_add(limit_of(n)) else {
            self.state = State::Poisoned;
            return Err(ReadError::OffsetOverflow);
        };
        let hit_limit = n >= self.max_line;
        let err = match self.buf.strip_suffix(b"\n") {
            Some(body) => match parse_line(body) {
                Ok(entry) => {
                    self.offset = next;
                    return Ok(Step::Entry {
                        offset: line_start,
                        entry,
                    });
                }
                Err(e) => e,
            },
            None if hit_limit => LineError::TooLong {
                limit: self.max_line,
            },
            None => LineError::Incomplete,
        };
        if !err.is_torn_candidate() {
            self.state = State::Poisoned;
            return Err(ReadError::Corrupt {
                at: line_start,
                source: err,
            });
        }
        // Torn candidate: is there a valid line after it?
        let scanned = if err == LineError::Incomplete {
            Ok((false, next))
        } else {
            let mut scratch = Vec::new();
            skip_rest_and_scan(
                &mut self.inner,
                &mut scratch,
                self.max_line,
                next,
                matches!(err, LineError::TooLong { .. }),
            )
        };
        match scanned {
            Err(e) => {
                self.state = State::Poisoned;
                Err(e.into())
            }
            Ok((true, _)) => {
                self.state = State::Poisoned;
                Err(ReadError::Corrupt {
                    at: line_start,
                    source: err,
                })
            }
            Ok((false, end)) => {
                let t = Tail::Torn {
                    at: line_start,
                    end,
                };
                self.state = State::Ended(t);
                Ok(Step::End(t))
            }
        }
    }
}

fn skip_rest_and_scan<R: BufRead>(
    inner: &mut R,
    scratch: &mut Vec<u8>,
    max_line: usize,
    start: u64,
    skip_first: bool,
) -> io::Result<(bool, u64)> {
    let mut pos = start;
    if skip_first {
        pos = pos
            .checked_add(skip_line(inner)?)
            .ok_or_else(|| io::Error::other("offset overflow"))?;
    }
    scan_rest(inner, scratch, max_line, pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::Kind;
    use crate::testutil::{file, sample, with_check};
    use std::io::Cursor;

    const ML: usize = 1 << 16;

    fn entries(n: u32) -> Vec<Entry<'static>> {
        (0..n).map(sample).collect()
    }

    /// Reads everything; returns entries (owned, with offsets) and the end or the error.
    fn read_all(
        data: &[u8],
        max_line: usize,
    ) -> (Vec<(u64, Entry<'static>)>, Result<Tail, ReadError>) {
        let mut r = CdxReader::open(data, max_line).unwrap();
        let mut out = Vec::new();
        loop {
            match r.next_step() {
                Ok(Step::Entry { offset, entry }) => out.push((offset, entry.into_owned())),
                Ok(Step::End(t)) => return (out, Ok(t)),
                Err(e) => return (out, Err(e)),
            }
        }
    }

    #[test]
    fn reader_roundtrip() {
        let es = entries(50);
        let (data, starts) = file(9, &es);
        let mut r = CdxReader::open(&data[..], ML).unwrap();
        assert_eq!(r.generation(), 9);
        for (i, e) in es.iter().enumerate() {
            match r.next_step().unwrap() {
                Step::Entry { offset, entry } => {
                    assert_eq!(offset, starts[i]);
                    assert_eq!(&entry, e);
                }
                Step::End(_) => panic!("early end"),
            }
        }
        let end = data.len() as u64;
        for _ in 0..2 {
            assert!(matches!(
                r.next_step().unwrap(),
                Step::End(Tail::Clean { end: e }) if e == end
            ));
        }
        assert_eq!(r.offset(), end);
    }

    #[test]
    fn reader_empty_after_header() {
        let (data, starts) = file(1, &[]);
        let (es, t) = read_all(&data, ML);
        assert!(es.is_empty());
        assert_eq!(t.unwrap(), Tail::Clean { end: starts[0] });
    }

    #[test]
    fn reader_header_errors() {
        let open = |d: &[u8]| CdxReader::open(d, ML).map(|_| ()).unwrap_err();
        assert!(matches!(open(b""), ReadError::MissingHeader));
        assert!(matches!(
            open(b"#masstuffy-cdx/1 gen=1"),
            ReadError::MissingHeader
        ));
        assert!(matches!(
            open(b"#masstuffy-cdx/2 gen=1\n"),
            ReadError::Header(HeaderError::UnsupportedVersion { version: 2 })
        ));
        assert!(matches!(
            open(&[b'x'; 100]),
            ReadError::Header(HeaderError::NotCdx)
        ));
        let mut d = b"#masstuffy-cdx/1 gen=1".to_vec();
        d.extend_from_slice(&[b' '; 100]);
        assert!(matches!(open(&d), ReadError::Header(HeaderError::Invalid)));
    }

    #[test]
    fn reader_torn_at_every_byte() {
        let es = entries(5);
        let (data, starts) = file(1, &es);
        let header_len = usize::try_from(starts[0]).unwrap();
        for c in header_len..=data.len() {
            let (got, t) = read_all(&data[..c], ML);
            let c64 = c as u64;
            let complete = starts[1..].iter().filter(|&&s| s <= c64).count();
            assert_eq!(got.len(), complete, "cut {c}");
            let b = *starts.iter().filter(|&&s| s <= c64).max().unwrap();
            let expect = if b == c64 {
                Tail::Clean { end: c64 }
            } else {
                Tail::Torn { at: b, end: c64 }
            };
            assert_eq!(t.unwrap(), expect, "cut {c}");
        }
    }

    #[test]
    fn reader_zero_tail_at_every_byte() {
        let es = entries(4);
        let (data, starts) = file(1, &es);
        let header_len = usize::try_from(starts[0]).unwrap();
        for c in header_len..data.len() {
            let c64 = c as u64;
            let b = *starts.iter().filter(|&&s| s <= c64).max().unwrap();
            let mut a = data.clone();
            a[c..].fill(0);
            let mut z = data[..c].to_vec();
            z.extend_from_slice(&[0u8; 4096]);
            for d in [a, z] {
                let (got, t) = read_all(&d, ML);
                let complete = starts[1..].iter().filter(|&&s| s <= b).count();
                assert_eq!(got.len(), complete, "cut {c}");
                match t.unwrap() {
                    Tail::Clean { end } => {
                        // zeros replacing nothing meaningful can only happen on a boundary
                        assert_eq!(b, c64);
                        assert_eq!(end, d.len() as u64);
                    }
                    Tail::Torn { at, end } => {
                        assert_eq!(at, b, "cut {c}");
                        assert_eq!(end, d.len() as u64);
                    }
                }
            }
        }
    }

    #[test]
    fn reader_bad_checksum_last_line_is_torn() {
        let (mut data, starts) = file(1, &entries(5));
        let last = usize::try_from(starts[4]).unwrap();
        data[last + 2] ^= 1;
        let (got, t) = read_all(&data, ML);
        assert_eq!(got.len(), 4);
        assert_eq!(
            t.unwrap(),
            Tail::Torn {
                at: starts[4],
                end: data.len() as u64
            }
        );
    }

    #[test]
    fn reader_bad_checksum_middle_is_corrupt() {
        let (mut data, starts) = file(1, &entries(5));
        data[usize::try_from(starts[2]).unwrap() + 2] ^= 1;
        let mut r = CdxReader::open(&data[..], ML).unwrap();
        assert!(matches!(r.next_step().unwrap(), Step::Entry { .. }));
        assert!(matches!(r.next_step().unwrap(), Step::Entry { .. }));
        assert!(matches!(
            r.next_step(),
            Err(ReadError::Corrupt { at, source: LineError::Checksum }) if at == starts[2]
        ));
        assert!(matches!(r.next_step(), Err(ReadError::Poisoned)));
    }

    #[test]
    fn reader_zeroed_middle_line_is_corrupt() {
        let (mut data, starts) = file(1, &entries(5));
        let (a, b) = (
            usize::try_from(starts[2]).unwrap(),
            usize::try_from(starts[3]).unwrap(),
        );
        data[a..b].fill(0);
        let (got, t) = read_all(&data, ML);
        assert_eq!(got.len(), 2);
        assert!(matches!(t, Err(ReadError::Corrupt { .. })));
    }

    #[test]
    fn reader_valid_checksum_bad_content_last_is_corrupt() {
        let (mut data, starts) = file(1, &entries(2));
        data.extend_from_slice(&with_check(
            "A 1 2 0 0123456789abcdef 2025-05-03T11:43:01Z response urn:x u",
        ));
        let (got, t) = read_all(&data, ML);
        assert_eq!(got.len(), 2);
        match t {
            Err(ReadError::Corrupt { at, source }) => {
                assert_eq!(at, starts[2]);
                assert_eq!(source, LineError::Entry(crate::EntryError::ZeroRawSize));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reader_overlong_tail_is_torn() {
        let es = entries(2);
        let (base, starts) = file(1, &es);
        let at = *starts.last().unwrap();
        let mut a = base.clone();
        a.extend_from_slice(&[b'a'; 10_000]);
        let (got, t) = read_all(&a, 300);
        assert_eq!(got.len(), 2);
        assert_eq!(
            t.unwrap(),
            Tail::Torn {
                at,
                end: a.len() as u64
            }
        );

        a.push(b'\n');
        let (_, t) = read_all(&a, 300);
        assert_eq!(
            t.unwrap(),
            Tail::Torn {
                at,
                end: a.len() as u64
            }
        );

        let mut v = a.clone();
        encode_valid(&mut v);
        let mut r = CdxReader::open(&v[..], 300).unwrap();
        loop {
            match r.next_step() {
                Ok(Step::Entry { .. }) => {}
                Err(ReadError::Corrupt {
                    at: x,
                    source: LineError::TooLong { .. },
                }) => {
                    assert_eq!(x, at);
                    break;
                }
                other => panic!("{other:?}"),
            }
        }
        assert!(r.buf_capacity() <= 1024);
    }

    fn encode_valid(v: &mut Vec<u8>) {
        crate::line::encode_entry(&sample(7), ML, v).unwrap();
    }

    #[test]
    fn reader_rejects_small_max_line() {
        let (data, _) = file(1, &entries(1));
        for ml in [0, 63] {
            assert!(matches!(
                CdxReader::open(&data[..], ml),
                Err(ReadError::InvalidMaxLine(_))
            ));
            assert!(matches!(
                CdxReader::resume(&data[..], 1, 0, ml),
                Err(ReadError::InvalidMaxLine(_))
            ));
        }
    }

    #[test]
    fn reader_resume() {
        let es = entries(6);
        let (data, _) = file(4, &es);
        let mut r = CdxReader::open(&data[..], ML).unwrap();
        for _ in 0..2 {
            assert!(matches!(r.next_step().unwrap(), Step::Entry { .. }));
        }
        let off = r.offset();
        let mut cur = Cursor::new(&data[..]);
        cur.set_position(off);
        let mut r2 = CdxReader::resume(cur, 4, off, ML).unwrap();
        for (i, e) in es.iter().enumerate().skip(2) {
            match r2.next_step().unwrap() {
                Step::Entry { offset, entry } => {
                    assert!(offset >= off);
                    assert_eq!(&entry, e, "{i}");
                }
                Step::End(_) => panic!(),
            }
        }
        assert!(matches!(
            r2.next_step().unwrap(),
            Step::End(Tail::Clean { .. })
        ));
    }

    #[test]
    fn reader_tombstone_kind() {
        let mut t = sample(1);
        t.kind = Kind::Tombstone;
        let (data, _) = file(1, &[sample(1), t]);
        let (got, _) = read_all(&data, ML);
        assert_eq!(got[1].1.kind, Kind::Tombstone);
    }

    #[test]
    fn reader_send() {
        fn assert_send<T: Send>() {}
        assert_send::<CdxReader<std::io::BufReader<std::fs::File>>>();
        assert_send::<crate::CdxWriter<std::fs::File>>();
        assert_send::<crate::EnvelopeReader<std::io::BufReader<std::fs::File>>>();
    }
}
