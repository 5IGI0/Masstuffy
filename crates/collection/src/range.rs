//! Bounded, line-aligned reading of a byte range of a CDX file.
//!
//! The range is known to be fsynced and line-aligned (a `Writer`'s `cdx_len()` or a checkpoint
//! offset), so a partial line is corruption or truncation, never a torn tail. It exists because
//! `CdxReader::next_step` returns an entry borrowing the reader, and a lending iterator that
//! skips lines cannot return such a borrow from inside a loop.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Take};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use masstuffy_cdx::{CdxReader, DEFAULT_MAX_LINE, Entry, LineError, parse_line};

use crate::error::Error;

const BUF: usize = 64 * 1024;

fn open_file(path: &Path) -> Result<(File, u64), Error> {
    let file = File::open(path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            Error::CdxMissing {
                path: path.to_path_buf(),
            }
        } else {
            Error::Io {
                op: "open",
                path: path.to_path_buf(),
                source: e,
            }
        }
    })?;
    let len = file
        .metadata()
        .map_err(|source| Error::Io {
            op: "stat",
            path: path.to_path_buf(),
            source,
        })?
        .len();
    Ok((file, len))
}

/// Opens `path`, checks that the header names `generation` and returns the file (positioned
/// anywhere), its length and the header length.
fn open_checked(path: &Path, generation: u32) -> Result<(File, u64, u64), Error> {
    let (file, file_len) = open_file(path)?;
    let r = CdxReader::open(BufReader::with_capacity(BUF, file), DEFAULT_MAX_LINE).map_err(
        |source| Error::CdxRead {
            path: path.to_path_buf(),
            source,
        },
    )?;
    if r.generation() != generation {
        return Err(Error::CdxGeneration {
            path: path.to_path_buf(),
            expected: generation,
            found: r.generation(),
        });
    }
    let header_len = r.offset();
    Ok((r.into_inner().into_inner(), file_len, header_len))
}

/// Length of the header line of the CDX at `path` (the offset of its first entry line).
///
/// # Errors
///
/// `CdxMissing`, `Io`, `CdxRead`, `CdxGeneration`.
pub(crate) fn cdx_header_len(path: &Path, generation: u32) -> Result<u64, Error> {
    open_checked(path, generation).map(|(_, _, h)| h)
}

/// Parses a CDX line, mapping a failure to `CdxCorrupt`.
pub(crate) fn parse_at<'l>(
    path: &Path,
    file_len: u64,
    at: u64,
    line: &'l [u8],
) -> Result<Entry<'l>, Error> {
    parse_line(line).map_err(|source: LineError| Error::CdxCorrupt {
        path: path.to_path_buf(),
        at,
        end: file_len,
        source,
    })
}

/// Reads the complete lines of `[from, end)` of a CDX file.
pub(crate) struct LineReader {
    inner: BufReader<Take<File>>,
    /// The last line, with its `\n`.
    buf: Vec<u8>,
    path: PathBuf,
    file_len: u64,
    /// Stream offset of the next line.
    next: u64,
    end: u64,
    /// `(offset, length without '\n')` of the last line returned.
    last: Option<(u64, usize)>,
}

impl LineReader {
    /// Opens `path`, checks the header names `generation`, positions at `from` (`None` = right
    /// after the header) and limits reading to `[from, end)`.
    pub(crate) fn open(
        path: &Path,
        generation: u32,
        from: Option<u64>,
        end: u64,
    ) -> Result<LineReader, Error> {
        let (file, file_len, header_len) = open_checked(path, generation)?;
        let from = from.unwrap_or(header_len);
        let range_err = || Error::CdxRange {
            path: path.to_path_buf(),
            from,
            end,
        };
        if from < header_len || end < from {
            return Err(range_err());
        }
        if end > file_len {
            return Err(Error::CdxTruncated {
                path: path.to_path_buf(),
                at: file_len,
                end,
            });
        }
        if from > header_len {
            let prev = from.checked_sub(1).ok_or_else(range_err)?;
            let mut b = [0u8; 1];
            file.read_exact_at(&mut b, prev)
                .map_err(|source| Error::Io {
                    op: "read",
                    path: path.to_path_buf(),
                    source,
                })?;
            if b != [b'\n'] {
                return Err(range_err());
            }
        }
        let mut f = &file;
        f.seek(SeekFrom::Start(from)).map_err(|source| Error::Io {
            op: "seek",
            path: path.to_path_buf(),
            source,
        })?;
        let limit = end
            .checked_sub(from)
            .ok_or(Error::Internal("cdx range length"))?;
        Ok(LineReader {
            inner: BufReader::with_capacity(BUF, file.take(limit)),
            buf: Vec::with_capacity(512),
            path: path.to_path_buf(),
            file_len,
            next: from,
            end,
            last: None,
        })
    }

    /// Reads the next complete line into the internal buffer and returns its offset, or `None`
    /// exactly at `end`. Read it with [`LineReader::current`] or
    /// [`LineReader::parse_current`].
    pub(crate) fn advance(&mut self) -> Result<Option<u64>, Error> {
        self.buf.clear();
        let cap = u64::try_from(DEFAULT_MAX_LINE).map_or(u64::MAX, |m| m.saturating_add(1));
        let n = (&mut self.inner)
            .take(cap)
            .read_until(b'\n', &mut self.buf)
            .map_err(|source| Error::Io {
                op: "read",
                path: self.path.clone(),
                source,
            })?;
        if n == 0 {
            self.last = None;
            return if self.next == self.end {
                Ok(None)
            } else {
                Err(Error::CdxTruncated {
                    path: self.path.clone(),
                    at: self.next,
                    end: self.end,
                })
            };
        }
        let n_u64 = u64::try_from(n).map_err(|_| Error::Internal("cdx line length"))?;
        let line_end = self
            .next
            .checked_add(n_u64)
            .ok_or(Error::Internal("cdx offset"))?;
        if self.buf.last() != Some(&b'\n') {
            self.last = None;
            return Err(if line_end == self.end {
                Error::CdxTruncated {
                    path: self.path.clone(),
                    at: self.next,
                    end: self.end,
                }
            } else {
                Error::CdxCorrupt {
                    path: self.path.clone(),
                    at: self.next,
                    end: self.file_len,
                    source: LineError::Incomplete,
                }
            });
        }
        let at = self.next;
        self.next = line_end;
        self.last = Some((at, n - 1));
        Ok(Some(at))
    }

    /// Stream offset of the next unread line.
    pub(crate) fn next_offset(&self) -> u64 {
        self.next
    }

    /// The line last returned by `advance`, without its `\n` (empty if none).
    pub(crate) fn current(&self) -> &[u8] {
        self.last
            .and_then(|(_, l)| self.buf.get(..l))
            .unwrap_or(&[])
    }

    /// Parses the current line, which `advance` returned at offset `at`.
    pub(crate) fn parse_current(&self, at: u64) -> Result<Entry<'_>, Error> {
        parse_at(&self.path, self.file_len, at, self.current())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        TestDir, cdx_lines, commit_batch, create_zstd, file_len, flip_byte, record,
    };

    /// A zstd collection with 5 single-record commits; returns the CDX path and its length.
    fn setup(d: &TestDir) -> (PathBuf, u64) {
        let dir = d.sub();
        let (mut c, _) = create_zstd(&dir);
        let mut p = c.preparer().unwrap();
        for i in 1..=5 {
            commit_batch(&mut c, &mut p, vec![record(i, 50)]);
        }
        let len = c.writer().cdx_len();
        (dir.join("index.1.cdx"), len)
    }

    fn offsets(r: &mut LineReader) -> Vec<u64> {
        let mut v = Vec::new();
        while let Some(at) = r.advance().unwrap() {
            r.parse_current(at).unwrap();
            v.push(at);
        }
        v
    }

    #[test]
    fn line_reader_ranges() {
        let d = TestDir::new();
        let (path, len) = setup(&d);
        let starts: Vec<u64> = cdx_lines(d.sub().as_path(), 1)
            .iter()
            .map(|l| l.0)
            .collect();
        assert_eq!(starts.len(), 6);
        let mut r = LineReader::open(&path, 1, None, len).unwrap();
        assert_eq!(offsets(&mut r), starts);
        let mut r = LineReader::open(&path, 1, Some(starts[3]), len).unwrap();
        assert_eq!(offsets(&mut r), starts[3..]);
        let mut r = LineReader::open(&path, 1, Some(len), len).unwrap();
        assert_eq!(r.advance().unwrap(), None);
        assert_eq!(cdx_header_len(&path, 1).unwrap(), starts[0]);
    }

    #[test]
    fn line_reader_bad_bounds() {
        let d = TestDir::new();
        let (path, len) = setup(&d);
        let starts: Vec<u64> = cdx_lines(d.sub().as_path(), 1)
            .iter()
            .map(|l| l.0)
            .collect();
        // `end` inside line 2: lines 0 and 1 are read, then the truncation is reported.
        let mut r = LineReader::open(&path, 1, None, starts[2] + 3).unwrap();
        assert!(r.advance().unwrap().is_some());
        assert!(r.advance().unwrap().is_some());
        assert!(matches!(
            r.advance(),
            Err(Error::CdxTruncated { at, end, .. }) if at == starts[2] && end == starts[2] + 3
        ));
        assert!(matches!(
            LineReader::open(&path, 1, None, file_len(&path) + 1),
            Err(Error::CdxTruncated { .. })
        ));
        assert!(matches!(
            LineReader::open(&path, 1, Some(starts[2] + 1), len),
            Err(Error::CdxRange { .. })
        ));
        assert!(matches!(
            LineReader::open(&path, 1, Some(1), len),
            Err(Error::CdxRange { .. })
        ));
        assert!(matches!(
            LineReader::open(&path, 1, Some(starts[3]), starts[2]),
            Err(Error::CdxRange { .. })
        ));
        assert!(matches!(
            LineReader::open(&path, 2, None, len),
            Err(Error::CdxGeneration {
                expected: 2,
                found: 1,
                ..
            })
        ));
        let missing = d.path().join("nope.cdx");
        assert!(matches!(
            LineReader::open(&missing, 1, None, len),
            Err(Error::CdxMissing { .. })
        ));
    }

    #[test]
    fn line_reader_corrupt_line() {
        let d = TestDir::new();
        let (path, len) = setup(&d);
        let starts: Vec<u64> = cdx_lines(d.sub().as_path(), 1)
            .iter()
            .map(|l| l.0)
            .collect();
        flip_byte(&path, starts[2] + 4);
        let mut r = LineReader::open(&path, 1, None, len).unwrap();
        for _ in 0..2 {
            let at = r.advance().unwrap().unwrap();
            r.parse_current(at).unwrap();
        }
        let at = r.advance().unwrap().unwrap();
        assert_eq!(at, starts[2]);
        assert!(matches!(
            r.parse_current(at),
            Err(Error::CdxCorrupt {
                at: a,
                source: LineError::Checksum,
                ..
            }) if a == starts[2]
        ));
    }
}
