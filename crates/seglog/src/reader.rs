//! The shared reader: positioned reads on a bounded cache of open segment files.

use std::fs::File;
use std::io;
use std::num::NonZeroUsize;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::error::{Error, Op};
use crate::naming::Naming;
use crate::{Locator, segment_path};

/// Default bound of the open-file cache.
pub const DEFAULT_MAX_OPEN_FILES: usize = 64;
/// Default largest accepted `Locator::len` (1 GiB).
pub const DEFAULT_MAX_READ_LEN: u64 = 1 << 30;

/// Options of a [`Reader`].
#[derive(Debug, Clone, Copy)]
pub struct ReaderOptions {
    /// Bound of the open-file cache (LRU). Lookups are linear scans: keep it small (<= ~1024).
    pub max_open_files: NonZeroUsize,
    /// Largest `Locator::len` accepted by `read_at` (locators come from disk: never trusted).
    pub max_read_len: u64,
}

impl Default for ReaderOptions {
    fn default() -> Self {
        let max_open_files = match NonZeroUsize::new(DEFAULT_MAX_OPEN_FILES) {
            Some(v) => v,
            None => NonZeroUsize::MIN,
        };
        ReaderOptions {
            max_open_files,
            max_read_len: DEFAULT_MAX_READ_LEN,
        }
    }
}

/// Shared reader of one segment log. Cheap to clone (one `Arc`); all clones share one cache.
/// `Send + Sync`.
///
/// Segments are opened on demand, so segments created later by an `Appender` are readable as soon
/// as their locator is known. Reads use positioned reads on shared handles: they never serialise
/// on a per-file lock. The only lock protects the slot table of the cache and is never held during
/// `open`, a read, `fstat` or `close`.
#[derive(Debug, Clone)]
pub struct Reader {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    dir: PathBuf,
    naming: Naming,
    max_read_len: u64,
    cache: Mutex<FileCache>,
}

#[derive(Debug)]
struct FileCache {
    cap: NonZeroUsize,
    tick: u64,
    /// `len() <= cap`.
    slots: Vec<Slot>,
}

#[derive(Debug)]
struct Slot {
    segment: u32,
    file: Arc<File>,
    last_used: u64,
}

impl FileCache {
    fn touch(&mut self) -> u64 {
        let t = self.tick;
        self.tick = self.tick.wrapping_add(1);
        t
    }

    fn get(&mut self, segment: u32) -> Option<Arc<File>> {
        let t = self.touch();
        let slot = self.slots.iter_mut().find(|s| s.segment == segment)?;
        slot.last_used = t;
        Some(Arc::clone(&slot.file))
    }

    /// Inserts `file`; returns the handle to use and a handle to drop outside the lock (the
    /// evicted one, or `file` itself if another thread inserted `segment` meanwhile).
    fn insert(&mut self, segment: u32, file: Arc<File>) -> (Arc<File>, Option<Arc<File>>) {
        if let Some(existing) = self.get(segment) {
            return (existing, Some(file));
        }
        let mut evicted = None;
        if self.slots.len() >= self.cap.get() {
            let victim = self
                .slots
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| s.last_used)
                .map(|(i, _)| i);
            if let Some(i) = victim {
                // `i` comes from `enumerate` on `slots` just above, so it is in range.
                evicted = Some(self.slots.swap_remove(i).file);
            }
        }
        let last_used = self.touch();
        self.slots.push(Slot {
            segment,
            file: Arc::clone(&file),
            last_used,
        });
        (file, evicted)
    }
}

impl Reader {
    /// Creates a reader. No I/O.
    pub fn new(dir: impl Into<PathBuf>, naming: Naming, opts: ReaderOptions) -> Reader {
        Reader {
            inner: Arc::new(Inner {
                dir: dir.into(),
                naming,
                max_read_len: opts.max_read_len,
                cache: Mutex::new(FileCache {
                    cap: opts.max_open_files,
                    tick: 0,
                    slots: Vec::new(),
                }),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, FileCache> {
        // Critical sections cannot panic, so poisoning only means a panic elsewhere; the slot
        // table is always consistent.
        self.inner
            .cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn path(&self, segment: u32) -> PathBuf {
        segment_path(&self.inner.dir, &self.inner.naming, segment)
    }

    fn handle(&self, segment: u32) -> Result<Arc<File>, Error> {
        if segment == 0 {
            return Err(Error::SegmentNotFound { segment });
        }
        let hit = self.lock().get(segment);
        if let Some(f) = hit {
            return Ok(f);
        }
        let path = self.path(segment);
        // Refuse symlinks, FIFOs, directories... before `open` (a FIFO would block it). The
        // window between this check and the open is accepted: the directory is trusted to be
        // owned by the repository. The post-open check below catches a swap into a non-file.
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_file() => {}
            Ok(_) => return Err(Error::NotAFile { path }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(Error::SegmentNotFound { segment });
            }
            Err(source) => {
                return Err(Error::Io {
                    op: Op::Stat,
                    path,
                    source,
                });
            }
        }
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(Error::SegmentNotFound { segment });
            }
            Err(source) => {
                return Err(Error::Io {
                    op: Op::Open,
                    path,
                    source,
                });
            }
        };
        match file.metadata() {
            Ok(m) if m.is_file() => {}
            Ok(_) => return Err(Error::NotAFile { path }),
            Err(source) => {
                return Err(Error::Io {
                    op: Op::Stat,
                    path,
                    source,
                });
            }
        }
        let (file, extra) = self.lock().insert(segment, Arc::new(file));
        // Dropping the last `Arc<File>` is a close(2): outside the lock.
        drop(extra);
        Ok(file)
    }

    /// Reads exactly `loc.len` bytes at `loc.offset` of segment `loc.segment` into `buf` (cleared,
    /// then resized; its capacity is reused, never shrunk). On error `buf` is left empty.
    ///
    /// A zero `len` (never produced by this crate's `Appender`) still requires the segment to
    /// exist and yields an empty `buf`.
    ///
    /// # Errors
    ///
    /// `ReadTooLarge` (`len` above `max_read_len` or `usize`), `OutOfBounds` (`offset + len`
    /// overflows or passes the end of the segment), `SegmentNotFound` (segment 0 or missing
    /// file), `NotAFile` (the segment path is not a regular file), `Io{Open|Stat|Read}`.
    pub fn read_at(&self, loc: &Locator, buf: &mut Vec<u8>) -> Result<(), Error> {
        buf.clear();
        let max = self.inner.max_read_len;
        let too_large = || Error::ReadTooLarge { len: loc.len, max };
        if loc.len > max {
            return Err(too_large());
        }
        let n = usize::try_from(loc.len).map_err(|_| too_large())?;
        let out_of_bounds = || Error::OutOfBounds {
            segment: loc.segment,
            offset: loc.offset,
            len: loc.len,
        };
        let end = loc.end().ok_or_else(out_of_bounds)?;
        let file = self.handle(loc.segment)?;
        // Check the length before allocating: a bogus locator must not cost `len` zeroed bytes.
        // A truncation racing with the read is still caught by `UnexpectedEof` below.
        let file_len = file
            .metadata()
            .map_err(|source| Error::Io {
                op: Op::Stat,
                path: self.path(loc.segment),
                source,
            })?
            .len();
        if end > file_len {
            return Err(out_of_bounds());
        }
        buf.resize(n, 0);
        if let Err(e) = file.read_exact_at(buf, loc.offset) {
            buf.clear();
            return Err(if e.kind() == io::ErrorKind::UnexpectedEof {
                out_of_bounds()
            } else {
                Error::Io {
                    op: Op::Read,
                    path: self.path(loc.segment),
                    source: e,
                }
            });
        }
        Ok(())
    }

    /// Current length of a segment (`fstat` on the cached handle).
    ///
    /// # Errors
    ///
    /// `SegmentNotFound`, `NotAFile`, `Io{Open|Stat}`.
    pub fn len_of(&self, segment: u32) -> Result<u64, Error> {
        let file = self.handle(segment)?;
        file.metadata()
            .map(|m| m.len())
            .map_err(|source| Error::Io {
                op: Op::Stat,
                path: self.path(segment),
                source,
            })
    }

    /// Drops the cached handle of `segment`, if any (in-flight reads keep their `Arc`).
    pub fn forget(&self, segment: u32) {
        let removed = {
            let mut c = self.lock();
            c.slots
                .iter()
                .position(|s| s.segment == segment)
                .map(|i| c.slots.swap_remove(i))
        };
        drop(removed);
    }

    /// Drops every cached handle (call after `remove_all` so the space of deleted files is
    /// reclaimed).
    pub fn forget_all(&self) {
        let removed = std::mem::take(&mut self.lock().slots);
        drop(removed);
    }

    /// The log directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// The naming of the segment files.
    #[must_use]
    pub fn naming(&self) -> &Naming {
        &self.inner.naming
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TestDir;
    use crate::{Appender, remove_all};
    use std::num::NonZeroU64;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn naming() -> Naming {
        Naming::new("seg.1", "warc").unwrap()
    }
    fn nz(x: u64) -> NonZeroU64 {
        NonZeroU64::new(x).unwrap()
    }
    fn opts(files: usize, max: u64) -> ReaderOptions {
        ReaderOptions {
            max_open_files: NonZeroUsize::new(files).unwrap(),
            max_read_len: max,
        }
    }
    fn cached(r: &Reader) -> Vec<u32> {
        let mut v: Vec<u32> = r.lock().slots.iter().map(|s| s.segment).collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn huge_len_past_eof_does_not_allocate() {
        let d = TestDir::new();
        let mut a = Appender::create(d.path(), naming(), nz(1 << 20)).unwrap();
        let locs = a.append_batch(&[b"hello".to_vec()]).unwrap();
        a.sync().unwrap();
        let r = Reader::new(d.path(), naming(), opts(4, 1 << 30));
        let mut buf = Vec::new();
        let loc = Locator {
            segment: 1,
            offset: locs[0].offset,
            len: 1 << 29,
        };
        assert!(matches!(
            r.read_at(&loc, &mut buf),
            Err(Error::OutOfBounds { .. })
        ));
        assert!(buf.capacity() < 1024);
    }

    #[test]
    fn reader_is_send_sync_clone() {
        fn s<T: Send + Sync + Clone>() {}
        s::<Reader>();
        fn t<T: Send>() {}
        t::<Appender>();
    }

    #[test]
    fn read_back_batches() {
        let d = TestDir::new();
        let mut a = Appender::create(d.path(), naming(), nz(10)).unwrap();
        let r = Reader::new(d.path(), naming(), ReaderOptions::default());
        let l1 = a.append_batch(&["abc", "de"]).unwrap();
        let l2 = a.append_batch(&["fghijk", "lm"]).unwrap();
        assert_ne!(l1[0].segment, l2[0].segment);
        let mut buf = Vec::new();
        for (l, want) in l1.iter().chain(&l2).zip(["abc", "de", "fghijk", "lm"]) {
            r.read_at(l, &mut buf).unwrap();
            assert_eq!(buf, want.as_bytes());
        }
    }

    #[test]
    fn read_errors() {
        let d = TestDir::new();
        let mut a = Appender::create(d.path(), naming(), nz(100)).unwrap();
        a.append_batch(&["abcd"]).unwrap();
        let r = Reader::new(d.path(), naming(), opts(4, 16));
        let mut buf = vec![1u8, 2];
        let len1 = r.len_of(1).unwrap();
        let cases = [
            (0, 0, 1, "nf"),
            (99, 0, 1, "nf"),
            (1, len1, 1, "oob"),
            (1, u64::MAX, 2, "oob"),
            (1, 0, 17, "big"),
        ];
        for (segment, offset, len, kind) in cases {
            let e = r
                .read_at(
                    &Locator {
                        segment,
                        offset,
                        len,
                    },
                    &mut buf,
                )
                .unwrap_err();
            match kind {
                "nf" => assert!(matches!(e, Error::SegmentNotFound { .. })),
                "oob" => assert!(matches!(e, Error::OutOfBounds { .. })),
                _ => assert!(matches!(e, Error::ReadTooLarge { .. })),
            }
            assert!(buf.is_empty());
            buf.push(9);
        }
    }

    #[test]
    fn buffer_reused() {
        let d = TestDir::new();
        let mut a = Appender::create(d.path(), naming(), nz(1000)).unwrap();
        let l = a.append_batch(&[vec![7u8; 100], vec![8u8; 10]]).unwrap();
        let r = Reader::new(d.path(), naming(), ReaderOptions::default());
        let mut buf = Vec::with_capacity(0);
        r.read_at(&l[0], &mut buf).unwrap();
        let (cap, ptr) = (buf.capacity(), buf.as_ptr());
        r.read_at(&l[1], &mut buf).unwrap();
        assert_eq!(buf.len(), 10);
        assert_eq!((buf.capacity(), buf.as_ptr()), (cap, ptr));
    }

    #[test]
    fn lru_eviction_order() {
        let d = TestDir::new();
        let mut a = Appender::create(d.path(), naming(), nz(2)).unwrap();
        let locs: Vec<Locator> = (0..3)
            .map(|_| a.append_batch(&["ab"]).unwrap()[0])
            .collect();
        let r = Reader::new(d.path(), naming(), opts(2, 100));
        let mut buf = Vec::new();
        for seg in [1usize, 2, 1, 3] {
            r.read_at(&locs[seg - 1], &mut buf).unwrap();
            assert!(r.lock().slots.len() <= 2);
        }
        assert_eq!(cached(&r), [1, 3]);
        r.read_at(&locs[1], &mut buf).unwrap();
        assert_eq!(buf, b"ab");
        assert_eq!(cached(&r), [2, 3]);
    }

    #[test]
    fn forget_and_remove() {
        let d = TestDir::new();
        let mut a = Appender::create(d.path(), naming(), nz(2)).unwrap();
        let l1 = a.append_batch(&["ab"]).unwrap()[0];
        let l2 = a.append_batch(&["cd"]).unwrap()[0];
        drop(a);
        let r = Reader::new(d.path(), naming(), ReaderOptions::default());
        let mut buf = Vec::new();
        r.read_at(&l1, &mut buf).unwrap();
        r.read_at(&l2, &mut buf).unwrap();
        r.forget(1);
        assert_eq!(cached(&r), [2]);
        r.read_at(&l1, &mut buf).unwrap();
        remove_all(d.path(), &naming()).unwrap();
        r.forget_all();
        assert!(cached(&r).is_empty());
        assert!(matches!(
            r.read_at(&l1, &mut buf),
            Err(Error::SegmentNotFound { .. })
        ));
    }

    #[test]
    fn rejects_non_regular_segments() {
        let d = TestDir::new();
        let r = Reader::new(d.path(), naming(), ReaderOptions::default());
        std::fs::write(d.path().join("target"), b"abc").unwrap();
        std::os::unix::fs::symlink(d.path().join("target"), d.path().join("seg.1.1.warc")).unwrap();
        assert!(matches!(r.len_of(1), Err(Error::NotAFile { .. })));
        let fifo = d.path().join("seg.1.2.warc");
        let ok = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            assert!(matches!(r.len_of(2), Err(Error::NotAFile { .. })));
        }
        assert!(cached(&r).is_empty());
    }

    #[test]
    fn simultaneous_miss_respects_cap() {
        let d = TestDir::new();
        let mut a = Appender::create(d.path(), naming(), nz(100)).unwrap();
        let l = a.append_batch(&["abc"]).unwrap()[0];
        for _ in 0..20 {
            let r = Reader::new(d.path(), naming(), opts(1, 100));
            let barrier = std::sync::Barrier::new(2);
            std::thread::scope(|sc| {
                for _ in 0..2 {
                    let (r, barrier) = (r.clone(), &barrier);
                    sc.spawn(move || {
                        let mut buf = Vec::new();
                        barrier.wait();
                        r.read_at(&l, &mut buf).unwrap();
                        assert_eq!(buf, b"abc");
                    });
                }
            });
            assert!(cached(&r).len() <= 1);
        }
    }

    #[test]
    fn len_of_tracks_appends() {
        let d = TestDir::new();
        let mut a = Appender::create(d.path(), naming(), nz(100)).unwrap();
        let r = Reader::new(d.path(), naming(), ReaderOptions::default());
        assert_eq!(r.len_of(1).unwrap(), 0);
        a.append_batch(&["abc"]).unwrap();
        assert_eq!(r.len_of(1).unwrap(), 3);
    }

    fn content(seed: u64, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| {
                let v = seed
                    .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                    .wrapping_add(u64::try_from(i).unwrap());
                u8::try_from(v >> 24 & 0xff).unwrap()
            })
            .collect()
    }

    fn lcg(s: &mut u64) -> u64 {
        *s = s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *s >> 33
    }

    #[test]
    fn concurrent_reads_while_appending() {
        let d = TestDir::new();
        let mut a = Appender::create(d.path(), naming(), nz(4096)).unwrap();
        let reader = Reader::new(d.path(), naming(), opts(2, 1 << 20));
        let done = AtomicBool::new(false);
        let list: Mutex<Vec<(Locator, u64)>> = Mutex::new(Vec::new());
        let batches: Mutex<Vec<Vec<Locator>>> = Mutex::new(Vec::new());

        std::thread::scope(|sc| {
            sc.spawn(|| {
                let mut rng = 1u64;
                for b in 0..300u32 {
                    let n = usize::try_from(lcg(&mut rng) % 8).unwrap() + 1;
                    let mut seeds = Vec::new();
                    let mut items = Vec::new();
                    for _ in 0..n {
                        let seed = lcg(&mut rng);
                        let len = if b % 40 == 7 {
                            2000
                        } else {
                            usize::try_from(lcg(&mut rng) % 1500).unwrap() + 1
                        };
                        seeds.push(seed);
                        items.push(content(seed, len));
                    }
                    let locs = a.append_batch(&items).unwrap();
                    {
                        let mut l = list.lock().unwrap();
                        l.extend(locs.iter().copied().zip(seeds.iter().copied()));
                    }
                    batches.lock().unwrap().push(locs);
                    if b % 10 == 9 {
                        a.sync().unwrap();
                    }
                }
                a.sync().unwrap();
                done.store(true, Ordering::Release);
            });
            for t in 0..4u64 {
                let r = reader.clone();
                let (done, list) = (&done, &list);
                sc.spawn(move || {
                    let mut rng = t + 100;
                    let mut buf = Vec::new();
                    loop {
                        let finished = done.load(Ordering::Acquire);
                        let snap: Vec<(Locator, u64)> = {
                            let l = list.lock().unwrap();
                            if l.is_empty() {
                                Vec::new()
                            } else {
                                (0..64)
                                    .map(|_| l[usize::try_from(lcg(&mut rng)).unwrap() % l.len()])
                                    .collect()
                            }
                        };
                        for (loc, seed) in snap {
                            r.read_at(&loc, &mut buf).unwrap();
                            assert_eq!(buf.len(), usize::try_from(loc.len).unwrap());
                            assert_eq!(buf, content(seed, buf.len()));
                        }
                        if finished {
                            break;
                        }
                    }
                });
            }
        });

        let fresh = reader.clone();
        let mut buf = Vec::new();
        for (loc, seed) in list.lock().unwrap().iter() {
            fresh.read_at(loc, &mut buf).unwrap();
            assert_eq!(buf, content(*seed, buf.len()));
        }
        let mut last = 0;
        for b in batches.lock().unwrap().iter() {
            let mut off = b[0].offset;
            for l in b {
                assert_eq!(l.segment, b[0].segment);
                assert_eq!(l.offset, off);
                off += l.len;
            }
            last = last.max(b[0].segment);
        }
        assert!(last > 10);
        for seg in 1..last {
            assert!(reader.len_of(seg).unwrap() > 0);
        }
    }
}
