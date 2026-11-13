//! Batch writer of index lines.

use std::io::Write;

use crate::entry::Entry;
use crate::error::{EntryError, WriteError};
use crate::line::{encode_entry, encode_header};

/// Encodes entries into one reusable buffer and hands it to `inner` in one `write_all` per batch.
///
/// Never fsyncs: the caller syncs the file. Durability order (spec 5): data fsync, then `push` and
/// [`write_pending`](Self::write_pending), then fsync of the CDX file (the commit point of the
/// batch). After a crash in between, the file may end with a partial line, zeros, or complete lines
/// of the unacknowledged batch.
///
/// A header queued by [`CdxWriter::new`] must be written and fsynced before the manifest commit
/// that activates the generation.
#[derive(Debug)]
pub struct CdxWriter<W> {
    inner: W,
    buf: Vec<u8>,
    max_line: usize,
    /// Length of the queued header (kept by `discard_pending`); 0 once written or absent.
    header_len: usize,
    poisoned: bool,
}

impl<W: Write> CdxWriter<W> {
    /// New file: queues the header line in the buffer (written by the next `write_pending`).
    pub fn new(inner: W, generation: u32, max_line: usize) -> Self {
        let mut buf = Vec::new();
        encode_header(generation, &mut buf);
        Self {
            inner,
            header_len: buf.len(),
            buf,
            max_line,
            poisoned: false,
        }
    }

    /// Existing file, positioned at its end (after a valid header and complete lines).
    pub fn append(inner: W, max_line: usize) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            max_line,
            header_len: 0,
            poisoned: false,
        }
    }

    /// Encodes one entry into the pending buffer (no I/O). Works on a poisoned writer too.
    ///
    /// # Errors
    /// See [`encode_entry`]; the buffer is unchanged on error.
    pub fn push(&mut self, entry: &Entry<'_>) -> Result<(), EntryError> {
        encode_entry(entry, self.max_line, &mut self.buf)
    }

    /// Bytes waiting in the buffer.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    /// Drops the pending entry lines (e.g. a batch abandoned before its write). A header queued
    /// by [`CdxWriter::new`] and not yet written is kept.
    pub fn discard_pending(&mut self) {
        self.buf.truncate(self.header_len);
    }

    /// `write_all(pending)` then `flush`, then clears the buffer (capacity kept). Nothing pending
    /// means no syscall.
    ///
    /// # Errors
    /// `Io` on failure, which poisons the writer (`Poisoned` from then on). Part of the batch may be
    /// on disk: the caller truncates the file back to its length before the batch, or lets
    /// recovery see a torn tail.
    pub fn write_pending(&mut self) -> Result<(), WriteError> {
        if self.poisoned {
            return Err(WriteError::Poisoned);
        }
        if self.buf.is_empty() {
            return Ok(());
        }
        let r = self
            .inner
            .write_all(&self.buf)
            .and_then(|()| self.inner.flush());
        if let Err(e) = r {
            self.poisoned = true;
            return Err(e.into());
        }
        self.buf.clear();
        self.header_len = 0;
        Ok(())
    }

    /// The underlying writer.
    #[must_use]
    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    /// The underlying writer, mutably (e.g. to `sync_all` a file).
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    /// Returns the underlying writer; pending bytes are discarded.
    #[must_use]
    pub fn into_inner(self) -> W {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::sample;
    use std::io;

    struct Counting {
        writes: usize,
        data: Vec<u8>,
    }
    impl Write for Counting {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            self.data.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct Failing;
    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("boom"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn writer_new_writes_header_then_entries() {
        let mut w = CdxWriter::new(Vec::new(), 7, 1 << 16);
        let mut expect = Vec::new();
        encode_header(7, &mut expect);
        for i in 0..3 {
            w.push(&sample(i)).unwrap();
            encode_entry(&sample(i), 1 << 16, &mut expect).unwrap();
        }
        w.write_pending().unwrap();
        assert_eq!(w.pending(), 0);
        assert_eq!(w.into_inner(), expect);
    }

    #[test]
    fn writer_one_write_per_batch() {
        let mut w = CdxWriter::append(
            Counting {
                writes: 0,
                data: Vec::new(),
            },
            1 << 16,
        );
        w.write_pending().unwrap();
        assert_eq!(w.get_ref().writes, 0);
        for i in 0..5 {
            w.push(&sample(i)).unwrap();
        }
        w.write_pending().unwrap();
        assert_eq!(w.get_ref().writes, 1);
        w.write_pending().unwrap();
        assert_eq!(w.get_ref().writes, 1);
    }

    #[test]
    fn writer_push_error_keeps_pending() {
        let mut w = CdxWriter::append(Vec::new(), 1 << 16);
        w.push(&sample(1)).unwrap();
        let p = w.pending();
        let mut bad = sample(2);
        bad.raw_size = 0;
        assert!(w.push(&bad).is_err());
        assert_eq!(w.pending(), p);
        w.write_pending().unwrap();
        let mut expect = Vec::new();
        encode_entry(&sample(1), 1 << 16, &mut expect).unwrap();
        assert_eq!(w.into_inner(), expect);
    }

    #[test]
    fn writer_poisoned_after_io_error() {
        let mut w = CdxWriter::append(Failing, 1 << 16);
        w.push(&sample(1)).unwrap();
        assert!(matches!(w.write_pending(), Err(WriteError::Io(_))));
        assert!(matches!(w.write_pending(), Err(WriteError::Poisoned)));
        assert!(w.push(&sample(2)).is_ok());
    }

    #[test]
    fn writer_discard_keeps_header() {
        let mut w = CdxWriter::new(Vec::new(), 3, 1 << 16);
        w.push(&sample(1)).unwrap();
        w.discard_pending();
        w.write_pending().unwrap();
        let mut expect = Vec::new();
        encode_header(3, &mut expect);
        assert_eq!(w.get_ref(), &expect);
        w.push(&sample(1)).unwrap();
        w.discard_pending();
        assert_eq!(w.pending(), 0);
    }

    #[test]
    fn writer_append_no_header() {
        let mut w = CdxWriter::append(Vec::new(), 1 << 16);
        w.push(&sample(1)).unwrap();
        w.write_pending().unwrap();
        assert!(!w.get_ref().starts_with(b"#"));
    }

    #[test]
    fn writer_reuses_buffer() {
        let mut w = CdxWriter::append(Vec::new(), 1 << 16);
        let batch = |w: &mut CdxWriter<Vec<u8>>| {
            for i in 0..5 {
                w.push(&sample(i)).unwrap();
            }
            w.write_pending().unwrap();
        };
        batch(&mut w);
        let cap = w.buf.capacity();
        batch(&mut w);
        assert_eq!(w.buf.capacity(), cap);
    }
}
