//! The single-writer appender.

use std::fs::{File, OpenOptions};
use std::io::{self, IoSlice, Seek, SeekFrom, Write};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use crate::error::{Error, Op, io};
use crate::naming::{Naming, scan};
use crate::{Locator, segment_path};

/// Single writer of one segment log. Not shared: every mutating method takes `&mut self`.
///
/// Invariants: `current >= 1`; segments `1..=current` exist; while not poisoned, `size` equals
/// the byte length of the current segment and the cursor position of `file`; every batch returned
/// so far lies in one segment (I9); `retired_unsynced` holds at most one file per rollover since
/// the last successful `sync`.
///
/// The file is opened without `O_APPEND`, with its cursor at `size`. The caller guarantees that
/// no other `Appender` writes the same log (see the crate docs).
#[derive(Debug)]
pub struct Appender {
    dir: PathBuf,
    naming: Naming,
    threshold: NonZeroU64,
    /// Number of the last segment.
    current: u32,
    /// Byte length of `current`.
    size: u64,
    file: File,
    /// `file` has bytes not yet covered by a successful `sync_data`.
    dirty: bool,
    /// Previous segments rolled away from while dirty; drained by `sync`.
    retired_unsynced: Vec<(u32, File)>,
    /// A segment file was created since the last successful sync.
    dir_dirty: bool,
    poisoned: bool,
    #[cfg(test)]
    fail_after: Option<u64>,
}

/// `Ok(true)` = roll over. See the crate docs for the rule.
///
/// # Errors
///
/// `BatchTooLarge` if `size + total` overflows.
fn place(size: u64, total: u64, threshold: NonZeroU64) -> Result<bool, Error> {
    let end = size.checked_add(total).ok_or(Error::BatchTooLarge)?;
    Ok(size > 0 && end > threshold.get())
}

/// Writes every byte of `bufs` with `write_vectored`, retrying on `Interrupted`.
/// `Ok(0)` from the writer becomes `WriteZero`.
fn write_all_vectored<W: Write>(w: &mut W, mut bufs: &mut [IoSlice<'_>]) -> io::Result<()> {
    while !bufs.is_empty() {
        match w.write_vectored(bufs) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            // `advance_slices` panics only if `n` exceeds the remaining length, which
            // `write_vectored` never returns.
            Ok(n) => IoSlice::advance_slices(&mut bufs, n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

impl Appender {
    fn new(
        dir: PathBuf,
        naming: Naming,
        threshold: NonZeroU64,
        current: u32,
        size: u64,
        file: File,
    ) -> Appender {
        Appender {
            dir,
            naming,
            threshold,
            current,
            size,
            file,
            dirty: false,
            retired_unsynced: Vec::new(),
            dir_dirty: false,
            poisoned: false,
            #[cfg(test)]
            fail_after: None,
        }
    }

    /// Creates a new empty log: `dir/<stem>.1.<ext>` (`create_new`, length 0), then fsyncs `dir`.
    /// `dir` must exist. After `Ok`, segment 1 durably exists.
    ///
    /// # Errors
    ///
    /// `AlreadyExists` if any segment of this log is present; `BadSegmentName`;
    /// `Io{ReadDir|Stat|Create}`; `Durable` (directory fsync).
    pub fn create(
        dir: impl Into<PathBuf>,
        naming: Naming,
        threshold: NonZeroU64,
    ) -> Result<Appender, Error> {
        let dir = dir.into();
        scan(&dir, &naming, &mut |_, _, path| {
            Err(Error::AlreadyExists {
                path: path.to_path_buf(),
            })
        })?;
        let path = segment_path(&dir, &naming, 1);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    Error::AlreadyExists { path: path.clone() }
                } else {
                    Error::Io {
                        op: Op::Create,
                        path: path.clone(),
                        source: e,
                    }
                }
            })?;
        masstuffy_durable::sync_dir(&dir)?;
        Ok(Appender::new(dir, naming, threshold, 1, 0, file))
    }

    /// Opens an existing log and continues appending at the end of its last segment.
    ///
    /// Bytes after the last indexed record (torn or orphan tail, I3) are kept: the next batch
    /// starts at the current file length. Stray entries (other stems or extensions, reserved
    /// names, index, manifest) are ignored. The threshold is not persisted: a last segment
    /// already larger than `threshold` simply rolls on the next batch.
    ///
    /// The returned appender is marked dirty: its first `sync` fsyncs the last segment and the
    /// directory, because an earlier writer may have created that segment without syncing.
    /// Data in earlier segments is never fsynced by a reopened appender.
    ///
    /// # Errors
    ///
    /// `NoSegments`; `Gap`; `BadSegmentName`; `NotAFile`; `Io{ReadDir|Stat|Open|Seek}`.
    pub fn open(
        dir: impl Into<PathBuf>,
        naming: Naming,
        threshold: NonZeroU64,
    ) -> Result<Appender, Error> {
        let dir = dir.into();
        let mut count: u64 = 0;
        let mut max: u32 = 0;
        scan(&dir, &naming, &mut |n, ft, path| {
            if !ft.is_file() {
                return Err(Error::NotAFile {
                    path: path.to_path_buf(),
                });
            }
            count = count.saturating_add(1);
            max = max.max(n);
            Ok(())
        })?;
        if count == 0 {
            return Err(Error::NoSegments {
                dir,
                stem: naming.stem().to_owned(),
                ext: naming.ext().to_owned(),
            });
        }
        // Names are canonical decimals (no duplicates): `count == max` <=> numbers are 1..=max.
        if count != u64::from(max) {
            return Err(Error::Gap { dir, count, max });
        }
        let path = segment_path(&dir, &naming, max);
        let mut file = OpenOptions::new()
            .write(true)
            .open(&path)
            .map_err(io(Op::Open, &path))?;
        let size = file.seek(SeekFrom::End(0)).map_err(io(Op::Seek, &path))?;
        let mut a = Appender::new(dir, naming, threshold, max, size, file);
        // A previous writer may have created this segment, or written to it, without ever
        // syncing: its directory entry and data are not known to be durable. The first `sync`
        // after `open` therefore fsyncs the last segment and the directory once.
        a.dirty = true;
        a.dir_dirty = true;
        Ok(a)
    }

    /// Appends `items` contiguously (I9) in one segment, per the rollover rule, and returns one
    /// locator per item, in order. The bytes are visible to readers but NOT durable until
    /// [`sync`](Appender::sync) returns `Ok`.
    ///
    /// An empty `items` does nothing. A partial write leaves an orphan tail on disk (I3), poisons
    /// the appender, and returns no locators. Once poisoned, every locator returned since the
    /// last successful `sync` must be treated as lost and re-appended after `open`: a reopened
    /// appender never fsyncs data in segments it did not write itself.
    ///
    /// # Errors
    ///
    /// Without I/O and without poisoning: `Poisoned`, `EmptyItem`, `BatchTooLarge`,
    /// `SegmentLimit`. Poisoning: `Io{Create|Write}`.
    pub fn append_batch<B: AsRef<[u8]>>(&mut self, items: &[B]) -> Result<Vec<Locator>, Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let mut total: u64 = 0;
        for (index, item) in items.iter().enumerate() {
            let len = u64::try_from(item.as_ref().len()).map_err(|_| Error::BatchTooLarge)?;
            if len == 0 {
                return Err(Error::EmptyItem { index });
            }
            total = total.checked_add(len).ok_or(Error::BatchTooLarge)?;
        }
        let roll = place(self.size, total, self.threshold)?;
        let (segment, base) = if roll {
            (self.current.checked_add(1).ok_or(Error::SegmentLimit)?, 0)
        } else {
            (self.current, self.size)
        };
        let new_size = base.checked_add(total).ok_or(Error::BatchTooLarge)?;

        let mut locators = Vec::with_capacity(items.len());
        let mut off = base;
        for item in items {
            // Cannot fail: each item length fits in u64 (checked above).
            let len = u64::try_from(item.as_ref().len()).map_err(|_| Error::BatchTooLarge)?;
            locators.push(Locator {
                segment,
                offset: off,
                len,
            });
            off = off.checked_add(len).ok_or(Error::BatchTooLarge)?;
        }

        if roll {
            let path = segment_path(&self.dir, &self.naming, segment);
            let new = match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(f) => f,
                Err(e) => {
                    self.poisoned = true;
                    return Err(Error::Io {
                        op: Op::Create,
                        path,
                        source: e,
                    });
                }
            };
            let old = std::mem::replace(&mut self.file, new);
            if self.dirty {
                self.retired_unsynced.push((self.current, old));
            }
            self.current = segment;
            self.size = 0;
            self.dirty = false;
            self.dir_dirty = true;
        }

        let mut slices: Vec<IoSlice<'_>> = items.iter().map(|i| IoSlice::new(i.as_ref())).collect();
        if let Err(e) = self.raw_write(&mut slices) {
            self.poisoned = true;
            return Err(Error::Io {
                op: Op::Write,
                path: segment_path(&self.dir, &self.naming, self.current),
                source: e,
            });
        }
        self.size = new_size;
        self.dirty = true;
        Ok(locators)
    }

    #[cfg(not(test))]
    fn raw_write(&mut self, slices: &mut [IoSlice<'_>]) -> io::Result<()> {
        write_all_vectored(&mut self.file, slices)
    }

    #[cfg(test)]
    fn raw_write(&mut self, slices: &mut [IoSlice<'_>]) -> io::Result<()> {
        match self.fail_after {
            Some(left) => {
                let mut w = tests::FailAfter {
                    inner: &mut self.file,
                    left,
                };
                write_all_vectored(&mut w, slices)
            }
            None => write_all_vectored(&mut self.file, slices),
        }
    }

    /// Makes every byte returned by previous `append_batch` calls durable, and the names of
    /// segment files created since the last sync.
    ///
    /// Order: `sync_data` each retired file (oldest first), then the current file if dirty, then
    /// the directory fsync if a segment was created. A no-op (no syscall) when nothing is pending.
    /// Any I/O error poisons the appender: after a failed fsync the kernel may have dropped the
    /// dirty pages, so a retry could falsely succeed.
    ///
    /// # Errors
    ///
    /// `Poisoned`; `Io{SyncData}`; `Durable` (directory fsync).
    pub fn sync(&mut self) -> Result<(), Error> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        match self.sync_inner() {
            Ok(()) => Ok(()),
            Err(e) => {
                self.poisoned = true;
                Err(e)
            }
        }
    }

    fn sync_inner(&mut self) -> Result<(), Error> {
        for (n, f) in &self.retired_unsynced {
            f.sync_data()
                .map_err(io(Op::SyncData, segment_path(&self.dir, &self.naming, *n)))?;
        }
        if self.dirty {
            self.file.sync_data().map_err(io(
                Op::SyncData,
                segment_path(&self.dir, &self.naming, self.current),
            ))?;
        }
        if self.dir_dirty {
            masstuffy_durable::sync_dir(&self.dir)?;
        }
        self.retired_unsynced.clear();
        self.dirty = false;
        self.dir_dirty = false;
        Ok(())
    }

    /// The log directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The naming of the segment files.
    #[must_use]
    pub fn naming(&self) -> &Naming {
        &self.naming
    }

    /// The rollover threshold.
    #[must_use]
    pub fn threshold(&self) -> NonZeroU64 {
        self.threshold
    }

    /// Number of the current (last) segment.
    #[must_use]
    pub fn current_segment(&self) -> u32 {
        self.current
    }

    /// Length in bytes of the current segment.
    #[must_use]
    pub fn current_len(&self) -> u64 {
        self.size
    }

    /// True after an I/O error: every later `append_batch` and `sync` fails with `Poisoned`.
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TestDir, names};
    use std::fs;

    /// Test writer that writes at most `left` bytes in total, then fails.
    pub(super) struct FailAfter<'a> {
        pub(super) inner: &'a mut File,
        pub(super) left: u64,
    }

    impl Write for FailAfter<'_> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.left == 0 {
                return Err(io::Error::other("injected"));
            }
            let n = buf
                .len()
                .min(usize::try_from(self.left).unwrap_or(usize::MAX));
            let w = self.inner.write(&buf[..n])?;
            self.left -= u64::try_from(w).unwrap();
            Ok(w)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn nz(x: u64) -> NonZeroU64 {
        NonZeroU64::new(x).unwrap()
    }
    fn naming() -> Naming {
        Naming::new("seg.1", "warc").unwrap()
    }
    fn file_bytes(d: &TestDir, name: &str) -> Vec<u8> {
        fs::read(d.path().join(name)).unwrap()
    }
    fn loc(segment: u32, offset: u64, len: u64) -> Locator {
        Locator {
            segment,
            offset,
            len,
        }
    }
    fn create(d: &TestDir, t: u64) -> Appender {
        Appender::create(d.path(), naming(), nz(t)).unwrap()
    }

    #[test]
    fn place_rule() {
        let t = nz(10);
        assert!(!place(0, 5, t).unwrap());
        assert!(!place(0, 25, t).unwrap());
        assert!(!place(5, 5, t).unwrap());
        assert!(place(5, 6, t).unwrap());
        assert!(place(5, 25, t).unwrap());
        assert!(matches!(place(u64::MAX, 1, t), Err(Error::BatchTooLarge)));
        assert!(matches!(place(1, u64::MAX, t), Err(Error::BatchTooLarge)));
    }

    #[test]
    fn create_makes_segment_one() {
        let d = TestDir::new();
        let a = create(&d, 10);
        assert_eq!(names(d.path()), ["seg.1.1.warc"]);
        assert_eq!(file_bytes(&d, "seg.1.1.warc").len(), 0);
        assert_eq!(a.current_segment(), 1);
        assert_eq!(a.current_len(), 0);
    }

    #[test]
    fn create_refuses_existing() {
        let d = TestDir::new();
        let _a = create(&d, 10);
        assert!(matches!(
            Appender::create(d.path(), naming(), nz(10)),
            Err(Error::AlreadyExists { .. })
        ));
        let d = TestDir::new();
        fs::write(d.path().join("seg.1.2.warc"), b"").unwrap();
        assert!(matches!(
            Appender::create(d.path(), naming(), nz(10)),
            Err(Error::AlreadyExists { .. })
        ));
        let d = TestDir::new();
        fs::write(d.path().join("seg.1.01.warc"), b"").unwrap();
        assert!(matches!(
            Appender::create(d.path(), naming(), nz(10)),
            Err(Error::BadSegmentName { .. })
        ));
    }

    #[test]
    fn batch_is_contiguous() {
        let d = TestDir::new();
        let mut a = create(&d, 1000);
        let l = a.append_batch(&["ab", "cde", "f"]).unwrap();
        assert_eq!(l, [loc(1, 0, 2), loc(1, 2, 3), loc(1, 5, 1)]);
        let l = a.append_batch(&["xy"]).unwrap();
        assert_eq!(l, [loc(1, 6, 2)]);
        assert_eq!(file_bytes(&d, "seg.1.1.warc"), b"abcdefxy");
        assert_eq!(a.current_len(), 8);
    }

    #[test]
    fn threshold_equal_fits() {
        let d = TestDir::new();
        let mut a = create(&d, 100);
        a.append_batch(&[[1u8; 60]]).unwrap();
        let l = a.append_batch(&[[2u8; 40]]).unwrap();
        assert_eq!(l, [loc(1, 60, 40)]);
        assert_eq!(a.current_len(), 100);
        assert_eq!(names(d.path()), ["seg.1.1.warc"]);
    }

    #[test]
    fn threshold_plus_one_rolls() {
        let d = TestDir::new();
        let mut a = create(&d, 100);
        a.append_batch(&[[1u8; 60]]).unwrap();
        let l = a.append_batch(&[[2u8; 41]]).unwrap();
        assert_eq!(l, [loc(2, 0, 41)]);
        assert_eq!(file_bytes(&d, "seg.1.1.warc").len(), 60);
        assert_eq!(file_bytes(&d, "seg.1.2.warc").len(), 41);
    }

    #[test]
    fn oversize_into_empty_segment() {
        let d = TestDir::new();
        let mut a = create(&d, 10);
        let l = a.append_batch(&[vec![0u8; 20], vec![1u8; 5]]).unwrap();
        assert_eq!(l, [loc(1, 0, 20), loc(1, 20, 5)]);
        assert_eq!(names(d.path()), ["seg.1.1.warc"]);
        let l = a.append_batch(&["z"]).unwrap();
        assert_eq!(l, [loc(2, 0, 1)]);
    }

    #[test]
    fn oversize_into_nonempty_segment() {
        let d = TestDir::new();
        let mut a = create(&d, 10);
        assert_eq!(a.append_batch(&[[0u8; 5]]).unwrap(), [loc(1, 0, 5)]);
        assert_eq!(a.append_batch(&[[0u8; 25]]).unwrap(), [loc(2, 0, 25)]);
        assert_eq!(a.append_batch(&["z"]).unwrap(), [loc(3, 0, 1)]);
        assert_eq!(file_bytes(&d, "seg.1.1.warc").len(), 5);
        assert_eq!(file_bytes(&d, "seg.1.2.warc").len(), 25);
        assert_eq!(file_bytes(&d, "seg.1.3.warc").len(), 1);
        assert_eq!(names(d.path()).len(), 3);
    }

    #[test]
    fn empty_batch_is_noop() {
        let d = TestDir::new();
        let mut a = create(&d, 10);
        assert!(a.append_batch::<&[u8]>(&[]).unwrap().is_empty());
        assert_eq!(names(d.path()), ["seg.1.1.warc"]);
        assert_eq!(a.current_len(), 0);
        assert!(!a.dirty);
    }

    #[test]
    fn empty_item_rejected_without_poison() {
        let d = TestDir::new();
        let mut a = create(&d, 10);
        let r = a.append_batch(&[b"a".as_slice(), b""]);
        assert!(matches!(r, Err(Error::EmptyItem { index: 1 })));
        assert_eq!(file_bytes(&d, "seg.1.1.warc").len(), 0);
        assert!(!a.is_poisoned());
        assert_eq!(a.append_batch(&["a"]).unwrap(), [loc(1, 0, 1)]);
    }

    #[test]
    fn reopen_continues_last_segment() {
        let d = TestDir::new();
        let mut a = create(&d, 10);
        for _ in 0..3 {
            a.append_batch(&[[0u8; 7]]).unwrap();
        }
        a.sync().unwrap();
        drop(a);
        let mut a = Appender::open(d.path(), naming(), nz(10)).unwrap();
        assert_eq!(a.current_segment(), 3);
        assert_eq!(a.current_len(), 7);
        assert_eq!(a.append_batch(&["ab"]).unwrap(), [loc(3, 7, 2)]);
    }

    #[test]
    fn reopen_after_orphan_tail() {
        let d = TestDir::new();
        let mut a = create(&d, 100);
        a.append_batch(&["abc"]).unwrap();
        a.sync().unwrap();
        drop(a);
        let mut f = OpenOptions::new()
            .append(true)
            .open(d.path().join("seg.1.1.warc"))
            .unwrap();
        f.write_all(b"zz").unwrap();
        drop(f);
        let mut a = Appender::open(d.path(), naming(), nz(100)).unwrap();
        assert_eq!(a.append_batch(&["q"]).unwrap(), [loc(1, 5, 1)]);
    }

    #[test]
    fn open_errors() {
        let open = |d: &TestDir| Appender::open(d.path(), naming(), nz(10)).unwrap_err();
        let d = TestDir::new();
        assert!(matches!(open(&d), Error::NoSegments { .. }));
        fs::write(d.path().join("seg.1.1.warc"), b"").unwrap();
        fs::write(d.path().join("seg.1.3.warc"), b"").unwrap();
        assert!(matches!(
            open(&d),
            Error::Gap {
                count: 2,
                max: 3,
                ..
            }
        ));
        let d = TestDir::new();
        fs::write(d.path().join("seg.1.2.warc"), b"").unwrap();
        assert!(matches!(
            open(&d),
            Error::Gap {
                count: 1,
                max: 2,
                ..
            }
        ));
        let d = TestDir::new();
        fs::write(d.path().join("seg.1.1.warc"), b"").unwrap();
        fs::write(d.path().join("seg.1.02.warc"), b"").unwrap();
        assert!(matches!(open(&d), Error::BadSegmentName { .. }));
        let d = TestDir::new();
        fs::create_dir(d.path().join("seg.1.1.warc")).unwrap();
        assert!(matches!(open(&d), Error::NotAFile { .. }));
    }

    #[test]
    fn open_marks_dirty_until_first_sync() {
        let d = TestDir::new();
        drop(create(&d, 10));
        let mut a = Appender::open(d.path(), naming(), nz(10)).unwrap();
        assert!(a.dirty && a.dir_dirty);
        a.sync().unwrap();
        assert!(!a.dirty && !a.dir_dirty);
    }

    #[test]
    fn open_ignores_strays() {
        let d = TestDir::new();
        fs::write(d.path().join("seg.1.1.warc"), b"").unwrap();
        for s in [
            "index.1.cdx",
            "seg.2.1.warc",
            "seg.1.1.warc.zst",
            "seg.1.x.warc",
            ".lock",
            ".tmp-1-2",
            "manifest.json",
        ] {
            fs::write(d.path().join(s), b"").unwrap();
        }
        let a = Appender::open(d.path(), naming(), nz(10)).unwrap();
        assert_eq!(a.current_segment(), 1);
    }

    #[test]
    fn sync_tracks_touched_segments() {
        let d = TestDir::new();
        let mut a = create(&d, 10);
        a.append_batch(&[[0u8; 5]]).unwrap();
        assert!(a.dirty);
        a.sync().unwrap();
        assert!(!a.dirty);
        a.append_batch(&[[0u8; 8]]).unwrap();
        assert_eq!(a.retired_unsynced.len(), 0);
        assert!(a.dir_dirty);
        a.append_batch(&[[0u8; 8]]).unwrap();
        assert_eq!(a.retired_unsynced.len(), 1);
        a.sync().unwrap();
        assert!(a.retired_unsynced.is_empty());
        assert!(!a.dirty && !a.dir_dirty);
        a.sync().unwrap();
    }

    #[test]
    fn write_failure_poisons() {
        let d = TestDir::new();
        let mut a = create(&d, 100);
        a.append_batch(&["hello"]).unwrap();
        a.fail_after = Some(3);
        let r = a.append_batch(&["abcdef"]);
        assert!(matches!(r, Err(Error::Io { op: Op::Write, .. })));
        assert!(a.is_poisoned());
        assert!(matches!(a.append_batch(&["x"]), Err(Error::Poisoned)));
        assert!(matches!(a.sync(), Err(Error::Poisoned)));
        drop(a);
        let mut a = Appender::open(d.path(), naming(), nz(100)).unwrap();
        assert_eq!(a.current_len(), 8);
        assert_eq!(a.append_batch(&["x"]).unwrap(), [loc(1, 8, 1)]);
        assert_eq!(&file_bytes(&d, "seg.1.1.warc")[..8], b"helloabc");
    }

    struct Mock {
        out: Vec<u8>,
        interrupted: bool,
        zero: bool,
    }
    impl Write for Mock {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.zero {
                return Ok(0);
            }
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::ErrorKind::Interrupted.into());
            }
            let n = buf.len().min(2);
            self.out.extend_from_slice(&buf[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn write_all_vectored_mock() {
        let mut m = Mock {
            out: Vec::new(),
            interrupted: false,
            zero: false,
        };
        let mut s = [
            IoSlice::new(b"abc"),
            IoSlice::new(b"defg"),
            IoSlice::new(b"h"),
        ];
        write_all_vectored(&mut m, &mut s).unwrap();
        assert_eq!(m.out, b"abcdefgh");
        let mut m = Mock {
            out: Vec::new(),
            interrupted: false,
            zero: true,
        };
        let mut s = [IoSlice::new(b"abc")];
        let e = write_all_vectored(&mut m, &mut s).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::WriteZero);
    }
}
