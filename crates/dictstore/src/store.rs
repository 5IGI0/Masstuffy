//! `DictStore`: scan, lookups, prepared decoders and encoders.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use masstuffy_zframe::{DecoderDict, EncoderDict, check_level};

use crate::MAX_REPORTED_IGNORED;
use crate::Options;
use crate::cache::{Cache, Key, Prepared, get_or_load};
use crate::error::{Error, Op, io as io_err};
use crate::file::{self, DictInfo};
use crate::name::{file_name, parse_file_name};

/// What [`DictStore::open`] skipped. Libraries do not log: the engine logs this.
#[derive(Debug, Default)]
pub struct ScanReport {
    /// Names not matching the file-name grammar (reserved names excluded): the first
    /// `MAX_REPORTED_IGNORED` in `read_dir` order.
    pub ignored: Vec<OsString>,
    /// Exact count of ignored names.
    pub ignored_count: u64,
    /// Number of `.tmp-*` leftovers removed.
    pub tmp_swept: usize,
}

/// Cache counters (monitoring and tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    /// Number of cached (ready) entries.
    pub entries: usize,
    /// Sum of their `mem_size`; always `<= budget`.
    pub bytes: u64,
    /// The byte budget.
    pub budget: u64,
    /// Number of dictionary files read for preparation since `open`.
    pub loads: u64,
}

/// The dictionary store of one repository.
///
/// `Send + Sync`: share it as `Arc<DictStore>`. All methods are blocking (file I/O, `fsync`, zstd
/// preparation): async callers use `spawn_blocking`. Id 0 means "no dictionary" and is never in the
/// store; frames without a dictionary use `Decompressor::decompress(.., None, ..)` and never reach
/// the store.
///
/// Two `DictStore` values on one directory, or files added by hand while a store is open, are
/// unsupported (the directory is owned by the store; new files are picked up at the next `open`).
///
/// # Locks
///
/// Lock order: `install_lock` then `files`. `cache` is never held with any other lock.
/// `decoder`/`encoder` take `cache`, release it, and then (inside the loader) take `files` for
/// reading only for a hash lookup.
pub struct DictStore {
    pub(crate) dir: PathBuf,
    pub(crate) opts: Options,
    /// Serialises `install` (held across its file I/O on purpose; lookups never take it).
    pub(crate) install_lock: Mutex<()>,
    /// `dict_id` to file. Only `install` inserts; nothing removes.
    pub(crate) files: RwLock<HashMap<u32, Arc<DictInfo>>>,
    cache: Mutex<Cache>,
    loads: AtomicU64,
}

impl DictStore {
    /// Opens the store in `dir`, which must exist. Blocking.
    ///
    /// Removes `.tmp-*` leftovers, then scans `<label>.<dict_id>.zstdict` files, reading only their
    /// 8-byte headers. Dictionaries are prepared lazily on first use. Names that do not match the
    /// grammar are ignored and reported; reserved names (`.lock`, `.tmp-*`, `.trash-*`) are skipped
    /// silently.
    ///
    /// The caller holds the repository `DirLock` and no other writer uses `dir`.
    ///
    /// # Errors
    ///
    /// Every problem with a file that matches the grammar is fatal, because it claims to be a
    /// dictionary and skipping it could hide an id that segments reference: [`Error::NotAFile`],
    /// [`Error::FileTooLarge`], [`Error::BadHeader`], [`Error::IdMismatch`] and
    /// [`Error::DuplicateId`] (invariant I6). A missing directory gives [`Error::Durable`]; other
    /// I/O failures give [`Error::Io`].
    pub fn open(dir: impl Into<PathBuf>, opts: Options) -> Result<(DictStore, ScanReport), Error> {
        let dir = dir.into();
        let mut report = ScanReport {
            tmp_swept: masstuffy_durable::sweep_tmp(&dir)?,
            ..ScanReport::default()
        };
        let mut files: HashMap<u32, Arc<DictInfo>> = HashMap::new();
        for entry in fs::read_dir(&dir).map_err(io_err(Op::ReadDir, &dir))? {
            let entry = entry.map_err(io_err(Op::ReadDir, &dir))?;
            let name = entry.file_name();
            if masstuffy_durable::is_reserved(&name) {
                continue;
            }
            let Some((label, name_id)) = name.to_str().and_then(parse_file_name) else {
                report.ignored_count = report.ignored_count.saturating_add(1);
                if report.ignored.len() < MAX_REPORTED_IGNORED {
                    report.ignored.push(name);
                }
                continue;
            };
            let path = entry.path();
            // `file_type` does not follow symlinks.
            let ty = entry.file_type().map_err(io_err(Op::Stat, &path))?;
            if !ty.is_file() {
                return Err(Error::NotAFile { path });
            }
            let (size, header_id) = file::read_header(&path, opts.max_file_size)?;
            if header_id != name_id {
                return Err(Error::IdMismatch {
                    path,
                    name_id,
                    header_id,
                });
            }
            let info = Arc::new(DictInfo {
                dict_id: name_id,
                label: label.into(),
                path,
                size,
            });
            match files.entry(name_id) {
                Entry::Vacant(v) => {
                    v.insert(info);
                }
                Entry::Occupied(o) => {
                    let (a, b) = (o.get().path.clone(), info.path.clone());
                    let (first, second) = if a <= b { (a, b) } else { (b, a) };
                    return Err(Error::DuplicateId {
                        dict_id: name_id,
                        first,
                        second,
                    });
                }
            }
        }
        let store = DictStore {
            dir,
            opts,
            install_lock: Mutex::new(()),
            files: RwLock::new(files),
            cache: Mutex::new(Cache::new(opts.cache_bytes)),
            loads: AtomicU64::new(0),
        };
        Ok((store, report))
    }

    /// The store directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The options the store was opened with.
    #[must_use]
    pub fn options(&self) -> Options {
        self.opts
    }

    pub(crate) fn files_read(&self) -> RwLockReadGuard<'_, HashMap<u32, Arc<DictInfo>>> {
        // Critical sections are plain map operations that cannot panic: poisoning is harmless.
        self.files.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn files_write(&self) -> RwLockWriteGuard<'_, HashMap<u32, Arc<DictInfo>>> {
        self.files.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the store has a dictionary with this id. O(1), no I/O.
    #[must_use]
    pub fn contains(&self, dict_id: u32) -> bool {
        self.files_read().contains_key(&dict_id)
    }

    /// The file of dictionary `dict_id`. O(1), no I/O, no allocation.
    #[must_use]
    pub fn get(&self, dict_id: u32) -> Option<Arc<DictInfo>> {
        self.files_read().get(&dict_id).cloned()
    }

    /// All dictionaries sorted by id. Allocates.
    #[must_use]
    pub fn list(&self) -> Vec<Arc<DictInfo>> {
        let mut v: Vec<_> = self.files_read().values().cloned().collect();
        v.sort_by_key(|i| i.dict_id);
        v
    }

    /// Reads and checks the file of `dict_id`, counting the load.
    fn read_dict(&self, dict_id: u32) -> Result<Vec<u8>, Error> {
        let info = self.get(dict_id).ok_or(Error::NotFound { dict_id })?;
        let bytes = file::read_full(&info, self.opts.max_file_size)?;
        self.loads.fetch_add(1, Ordering::Relaxed);
        Ok(bytes)
    }

    /// The prepared decoder of `dict_id`, cached. Blocking on a miss (file read and preparation).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown id (including 0); [`Error::Io`], [`Error::NotAFile`],
    /// [`Error::FileTooLarge`], [`Error::BadHeader`], [`Error::IdMismatch`] if the file is gone or
    /// changed; [`Error::Codec`] if zstd refuses it. Errors are not cached.
    pub fn decoder(&self, dict_id: u32) -> Result<Arc<DecoderDict>, Error> {
        if dict_id == 0 {
            return Err(Error::NotFound { dict_id });
        }
        let p = get_or_load(&self.cache, Key::Dec(dict_id), || {
            let bytes = self.read_dict(dict_id)?;
            let d = DecoderDict::new(&bytes).map_err(|source| Error::Codec { dict_id, source })?;
            Ok(Prepared::Dec(Arc::new(d)))
        })?;
        match p {
            Prepared::Dec(d) => Ok(d),
            // Impossible: a `Dec` key only ever holds a `Dec` value.
            Prepared::Enc(_) => Err(Error::NotFound { dict_id }),
        }
    }

    /// The prepared encoder of `dict_id` for `level`, cached by `(dict_id, level)`. Blocking on a
    /// miss.
    ///
    /// # Errors
    ///
    /// As [`DictStore::decoder`], and [`Error::Codec`] with `InvalidLevel` for a level zstd does
    /// not support (checked before the cache is touched).
    pub fn encoder(&self, dict_id: u32, level: i32) -> Result<Arc<EncoderDict>, Error> {
        if dict_id == 0 {
            return Err(Error::NotFound { dict_id });
        }
        check_level(level).map_err(|source| Error::Codec { dict_id, source })?;
        let p = get_or_load(&self.cache, Key::Enc(dict_id, level), || {
            let bytes = self.read_dict(dict_id)?;
            let e = EncoderDict::new(&bytes, level)
                .map_err(|source| Error::Codec { dict_id, source })?;
            Ok(Prepared::Enc(Arc::new(e)))
        })?;
        match p {
            Prepared::Enc(e) => Ok(e),
            // Impossible: an `Enc` key only ever holds an `Enc` value.
            Prepared::Dec(_) => Err(Error::NotFound { dict_id }),
        }
    }

    /// Cache counters.
    #[must_use]
    pub fn cache_stats(&self) -> CacheStats {
        let (entries, bytes, budget) = self
            .cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .stats();
        CacheStats {
            entries,
            bytes,
            budget,
            loads: self.loads.load(Ordering::Relaxed),
        }
    }

    /// Path of the file for `label` and `id` (used by `install`).
    pub(crate) fn path_for(&self, label: &str, id: u32) -> (String, PathBuf) {
        let name = file_name(label, id);
        let path = self.dir.join(&name);
        (name, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TestDir, base_dict, names, put, variant, with_id};
    use masstuffy_zframe::{Compressor, Decompressor, DictError, inspect_single_frame};
    use std::os::unix::ffi::OsStrExt;
    use std::sync::Barrier;
    use std::thread;

    fn open(dir: &Path) -> DictStore {
        DictStore::open(dir, Options::default()).unwrap().0
    }

    fn open_with(dir: &Path, cache_bytes: u64) -> DictStore {
        let opts = Options {
            cache_bytes,
            ..Options::default()
        };
        DictStore::open(dir, opts).unwrap().0
    }

    fn dec_size(bytes: &[u8]) -> u64 {
        DecoderDict::new(bytes).unwrap().mem_size()
    }

    /// Places `variant(n)` as `<label>.<id>.zstdict`.
    fn place(dir: &Path, label: &str, id: u32, n: u32) -> Vec<u8> {
        let d = with_id(variant(n), id);
        put(dir, &file_name(label, id), &d);
        d
    }

    #[test]
    fn open_empty_dir() {
        let t = TestDir::new();
        let (s, r) = DictStore::open(t.path(), Options::default()).unwrap();
        assert!(s.list().is_empty());
        assert_eq!((r.ignored_count, r.ignored.len(), r.tmp_swept), (0, 0, 0));
    }

    #[test]
    fn open_missing_dir_fails() {
        let t = TestDir::new();
        let e = DictStore::open(t.path().join("nope"), Options::default())
            .err()
            .unwrap();
        assert!(matches!(e, Error::Durable(_)), "{e:?}");
    }

    #[test]
    fn open_valid_files() {
        let t = TestDir::new();
        let a = place(t.path(), "a", 40000, 1);
        let b = place(t.path(), "b", 40001, 2);
        let s = open(t.path());
        let l = s.list();
        assert_eq!(
            l.iter().map(|i| i.dict_id()).collect::<Vec<_>>(),
            [40000, 40001]
        );
        assert_eq!(l.iter().map(|i| i.label()).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(l[0].size(), u64::try_from(a.len()).unwrap());
        assert_eq!(l[1].size(), u64::try_from(b.len()).unwrap());
        assert_eq!(l[0].path(), t.path().join("a.40000.zstdict"));
        assert!(s.contains(40000) && !s.contains(40002));
        assert!(s.get(40001).is_some() && s.get(40002).is_none());
        assert_eq!(s.cache_stats().loads, 0);
    }

    #[test]
    fn open_name_header_mismatch() {
        let t = TestDir::new();
        put(t.path(), "a.40001.zstdict", &with_id(variant(1), 40000));
        let e = DictStore::open(t.path(), Options::default()).err().unwrap();
        assert!(
            matches!(
                e,
                Error::IdMismatch {
                    name_id: 40001,
                    header_id: 40000,
                    ..
                }
            ),
            "{e:?}"
        );
    }

    #[test]
    fn open_duplicate_id_fatal() {
        let t = TestDir::new();
        place(t.path(), "a", 40000, 1);
        place(t.path(), "b", 40000, 2);
        let e = DictStore::open(t.path(), Options::default()).err().unwrap();
        match e {
            Error::DuplicateId {
                dict_id,
                first,
                second,
            } => {
                assert_eq!(dict_id, 40000);
                assert_eq!(first, t.path().join("a.40000.zstdict"));
                assert_eq!(second, t.path().join("b.40000.zstdict"));
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn open_bad_header() {
        let valid = with_id(variant(1), 40000);
        let mut bad_magic = valid.clone();
        bad_magic[0] ^= 1;
        let mut zero_id = valid.clone();
        zero_id[4..8].copy_from_slice(&[0; 4]);
        type Check = fn(&DictError) -> bool;
        let cases: [(Vec<u8>, Check); 3] = [
            (valid[..4].to_vec(), |e| *e == DictError::Truncated),
            (bad_magic, |e| matches!(e, DictError::BadMagic(_))),
            (zero_id, |e| *e == DictError::ZeroId),
        ];
        for (bytes, check) in cases {
            let t = TestDir::new();
            put(t.path(), "bad.40000.zstdict", &bytes);
            let e = DictStore::open(t.path(), Options::default()).err().unwrap();
            assert!(
                matches!(&e, Error::BadHeader { source, .. } if check(source)),
                "{e:?}"
            );
        }
    }

    #[test]
    fn open_hostile_names_ignored() {
        let t = TestDir::new();
        let d = with_id(variant(1), 40000);
        for name in [
            "a.b.c.zstdict",
            "noext",
            "x.40000.zstdict.bak",
            "x.040000.zstdict",
            "x.0.zstdict",
            "x.4294967296.zstdict",
            "x.+12.zstdict",
            "lbl with space.40000.zstdict",
            "x.40000.ZSTDICT",
        ] {
            put(t.path(), name, &d);
        }
        fs::write(
            t.path()
                .join(std::ffi::OsStr::from_bytes(b"\xff.40000.zstdict")),
            &d,
        )
        .unwrap();
        put(t.path(), ".trash-x", &d);
        put(t.path(), ".lock", b"");
        let (s, r) = DictStore::open(t.path(), Options::default()).unwrap();
        assert!(s.list().is_empty());
        assert_eq!((r.ignored_count, r.ignored.len()), (10, 10));
        assert!(r.ignored.iter().all(|n| !n.as_bytes().starts_with(b".")));
    }

    #[test]
    fn open_ignored_report_capped() {
        let t = TestDir::new();
        for i in 0..70 {
            put(t.path(), &format!("junk-{i}"), b"x");
        }
        let (_, r) = DictStore::open(t.path(), Options::default()).unwrap();
        assert_eq!((r.ignored_count, r.ignored.len()), (70, 64));
    }

    #[test]
    fn open_sweeps_tmp() {
        let t = TestDir::new();
        put(t.path(), ".tmp-999-1", b"partial");
        place(t.path(), "a", 40000, 1);
        let (s, r) = DictStore::open(t.path(), Options::default()).unwrap();
        assert_eq!(names(t.path()), ["a.40000.zstdict"]);
        assert_eq!(r.tmp_swept, 1);
        assert!(s.contains(40000));
    }

    #[test]
    fn open_not_a_file() {
        let t = TestDir::new();
        fs::create_dir(t.path().join("d.40000.zstdict")).unwrap();
        let e = DictStore::open(t.path(), Options::default()).err().unwrap();
        assert!(matches!(e, Error::NotAFile { .. }), "{e:?}");

        let outside = TestDir::new();
        place(outside.path(), "real", 40001, 1);
        let t = TestDir::new();
        std::os::unix::fs::symlink(
            outside.path().join("real.40001.zstdict"),
            t.path().join("s.40001.zstdict"),
        )
        .unwrap();
        let e = DictStore::open(t.path(), Options::default()).err().unwrap();
        assert!(matches!(e, Error::NotAFile { .. }), "{e:?}");
    }

    #[test]
    fn open_oversize_file() {
        let opts = Options {
            max_file_size: 1024,
            ..Options::default()
        };
        let header = &with_id(variant(1), 40000)[..8];
        for (len, ok) in [(1025u64, false), (1024, true)] {
            let t = TestDir::new();
            put(t.path(), "big.40000.zstdict", header);
            fs::OpenOptions::new()
                .write(true)
                .open(t.path().join("big.40000.zstdict"))
                .unwrap()
                .set_len(len)
                .unwrap();
            let r = DictStore::open(t.path(), opts);
            if ok {
                assert!(r.unwrap().0.contains(40000));
            } else {
                let e = r.err().unwrap();
                assert!(
                    matches!(
                        e,
                        Error::FileTooLarge {
                            size: 1025,
                            max: 1024,
                            ..
                        }
                    ),
                    "{e:?}"
                );
            }
        }
    }

    // --- cache ---

    #[test]
    fn decoder_cached() {
        let t = TestDir::new();
        let d = place(t.path(), "a", 40000, 1);
        let s = open(t.path());
        let x = s.decoder(40000).unwrap();
        let y = s.decoder(40000).unwrap();
        assert!(Arc::ptr_eq(&x, &y));
        assert_eq!(x.id(), 40000);
        let st = s.cache_stats();
        assert_eq!((st.loads, st.entries, st.bytes), (1, 1, dec_size(&d)));
    }

    #[test]
    fn decoder_not_found() {
        let t = TestDir::new();
        let s = open(t.path());
        for e in [
            s.decoder(0).err().unwrap(),
            s.decoder(12345).err().unwrap(),
            s.encoder(0, 3).err().unwrap(),
            s.encoder(12345, 3).err().unwrap(),
        ] {
            assert!(matches!(e, Error::NotFound { .. }), "{e:?}");
        }
        assert_eq!(s.cache_stats().entries, 0);
    }

    #[test]
    fn encoder_bad_level() {
        let t = TestDir::new();
        place(t.path(), "a", 40000, 1);
        let s = open(t.path());
        let e = s.encoder(40000, 1000).err().unwrap();
        assert!(
            matches!(
                e,
                Error::Codec {
                    source: masstuffy_zframe::CodecError::InvalidLevel(1000),
                    ..
                }
            ),
            "{e:?}"
        );
        let st = s.cache_stats();
        assert_eq!((st.loads, st.entries), (0, 0));
    }

    #[test]
    fn encoder_keyed_by_level() {
        let t = TestDir::new();
        place(t.path(), "a", 40000, 1);
        let s = open(t.path());
        let e3 = s.encoder(40000, 3).unwrap();
        let e19 = s.encoder(40000, 19).unwrap();
        assert!(!Arc::ptr_eq(&e3, &e19));
        assert_eq!((e3.level(), e19.level()), (3, 19));
        assert_eq!(s.cache_stats().entries, 2);
        assert!(Arc::ptr_eq(&e3, &s.encoder(40000, 3).unwrap()));
        assert_eq!(s.cache_stats().loads, 2);
    }

    #[test]
    fn cache_bound_and_lru() {
        let t = TestDir::new();
        let a = place(t.path(), "a", 40000, 1);
        let b = place(t.path(), "b", 40001, 2);
        place(t.path(), "c", 40002, 3);
        let budget = dec_size(&a) + dec_size(&b) + 1;
        let s = open_with(t.path(), budget);
        let check = |s: &DictStore| {
            let st = s.cache_stats();
            assert!(st.bytes <= budget, "{st:?}");
        };
        s.decoder(40000).unwrap();
        check(&s);
        let b_arc = s.decoder(40001).unwrap();
        check(&s);
        s.decoder(40000).unwrap();
        s.decoder(40002).unwrap();
        check(&s);
        let st = s.cache_stats();
        assert_eq!((st.entries, st.loads), (2, 3));
        s.decoder(40000).unwrap();
        assert_eq!(s.cache_stats().loads, 3);
        s.decoder(40001).unwrap();
        assert_eq!(s.cache_stats().loads, 4);
        check(&s);
        // The evicted Arc still works.
        let enc = EncoderDict::new(&with_id(variant(2), 40001), 3).unwrap();
        let (mut c, mut dc) = (Compressor::new().unwrap(), Decompressor::new().unwrap());
        let (mut f, mut back) = (Vec::new(), Vec::new());
        c.compress(b"hello hello", &enc, &mut f).unwrap();
        dc.decompress(&f, Some(&b_arc), 1 << 20, &mut back).unwrap();
        assert_eq!(back, b"hello hello");
    }

    #[test]
    fn oversize_and_zero_budget_not_cached() {
        for budget in [1, 0] {
            let t = TestDir::new();
            place(t.path(), "a", 40000, 1);
            let s = open_with(t.path(), budget);
            s.decoder(40000).unwrap();
            let st = s.cache_stats();
            assert_eq!((st.entries, st.bytes, st.loads), (0, 0, 1));
            s.decoder(40000).unwrap();
            assert_eq!(s.cache_stats().loads, 2);
        }
    }

    #[test]
    fn load_error_not_cached() {
        let t = TestDir::new();
        let s = open(t.path());
        let id = s.install("a", &variant(1)).unwrap().dict_id;
        let mut junk = with_id(variant(1), id)[..8].to_vec();
        junk.extend_from_slice(&[0xAB; 300]);
        fs::write(t.path().join(file_name("a", id)), &junk).unwrap();
        for _ in 0..2 {
            let e = s.decoder(id).err().unwrap();
            assert!(matches!(e, Error::Codec { .. }), "{e:?}");
            assert_eq!(s.cache_stats().entries, 0);
        }
    }

    #[test]
    fn concurrent_load_error_releases_waiters() {
        let t = TestDir::new();
        let s = open(t.path());
        let id = s.install("a", &variant(1)).unwrap().dict_id;
        let mut junk = with_id(variant(1), id)[..8].to_vec();
        junk.extend_from_slice(&[0xAB; 300]);
        fs::write(t.path().join(file_name("a", id)), &junk).unwrap();
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|sc| {
            for _ in 0..8 {
                sc.spawn(|| {
                    barrier.wait();
                    let e = s.decoder(id).err().unwrap();
                    assert!(matches!(e, Error::Codec { .. }), "{e:?}");
                });
            }
        });
        assert_eq!(s.cache_stats().entries, 0);
    }

    #[test]
    fn file_swapped_header_mismatch() {
        let t = TestDir::new();
        place(t.path(), "a", 40000, 1);
        let s = open(t.path());
        put(t.path(), "a.40000.zstdict", &with_id(variant(1), 40001));
        let e = s.decoder(40000).err().unwrap();
        assert!(
            matches!(
                e,
                Error::IdMismatch {
                    name_id: 40000,
                    header_id: 40001,
                    ..
                }
            ),
            "{e:?}"
        );
    }

    #[test]
    fn file_grew_beyond_cap() {
        let t = TestDir::new();
        let d = place(t.path(), "a", 40000, 1);
        let len = u64::try_from(d.len()).unwrap();
        let opts = Options {
            max_file_size: len + 10,
            ..Options::default()
        };
        let (s, _) = DictStore::open(t.path(), opts).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(t.path().join("a.40000.zstdict"))
            .unwrap()
            .set_len(len + 100)
            .unwrap();
        let e = s.decoder(40000).err().unwrap();
        assert!(matches!(e, Error::FileTooLarge { .. }), "{e:?}");
    }

    // --- concurrency ---

    #[test]
    fn concurrent_same_key_single_load() {
        let t = TestDir::new();
        place(t.path(), "a", 40000, 1);
        place(t.path(), "b", 40001, 2);
        let s = open(t.path());
        let barrier = Barrier::new(8);
        let encs: Vec<_> = thread::scope(|sc| {
            let hs: Vec<_> = (0..8)
                .map(|_| {
                    sc.spawn(|| {
                        barrier.wait();
                        s.encoder(40000, 19).unwrap()
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(encs.iter().all(|e| Arc::ptr_eq(e, &encs[0])));
        assert_eq!(s.cache_stats().loads, 1);
        let barrier = Barrier::new(8);
        let decs: Vec<_> = thread::scope(|sc| {
            let hs: Vec<_> = (0..8)
                .map(|_| {
                    sc.spawn(|| {
                        barrier.wait();
                        s.decoder(40001).unwrap()
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(decs.iter().all(|d| Arc::ptr_eq(d, &decs[0])));
        assert_eq!(s.cache_stats().loads, 2);
    }

    #[test]
    fn concurrent_mixed() {
        let t = TestDir::new();
        let s = open_with(
            t.path(),
            2 * EncoderDict::new(base_dict(), 3).unwrap().mem_size(),
        );
        let ids: Vec<u32> = (0..4)
            .map(|n| s.install(&format!("p{n}"), &variant(n)).unwrap().dict_id)
            .collect();
        let budget = s.cache_stats().budget;
        thread::scope(|sc| {
            for _ in 0..6 {
                sc.spawn(|| {
                    for k in 0..50 {
                        let id = ids[k % 4];
                        assert_eq!(s.decoder(id).unwrap().id(), id);
                        assert_eq!(s.encoder(id, 3).unwrap().id(), id);
                    }
                });
            }
            for w in 0..2u32 {
                let s = &s;
                sc.spawn(move || {
                    for j in 0..3 {
                        let n = 10 + w * 10 + j;
                        s.install(&format!("n{n}"), &variant(n)).unwrap();
                    }
                });
            }
        });
        assert!(s.cache_stats().bytes <= budget);
        let all = s.list();
        assert_eq!(all.len(), 10);
        for i in all {
            assert_eq!(s.decoder(i.dict_id()).unwrap().id(), i.dict_id());
        }
    }

    #[test]
    fn roundtrip_with_installed() {
        let t = TestDir::new();
        let s = open(t.path());
        let id = s.install("a", &variant(1)).unwrap().dict_id;
        let enc = s.encoder(id, 3).unwrap();
        let dec = s.decoder(id).unwrap();
        let sample = b"WARC/1.1\r\nWARC-Type: response\r\n\r\nhello";
        let (mut c, mut dc) = (Compressor::new().unwrap(), Decompressor::new().unwrap());
        let (mut f, mut back) = (Vec::new(), Vec::new());
        c.compress(sample, &enc, &mut f).unwrap();
        assert_eq!(inspect_single_frame(&f).unwrap().dict_id, id);
        dc.decompress(&f, Some(&dec), 1 << 20, &mut back).unwrap();
        assert_eq!(back, sample);
    }

    #[test]
    fn store_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DictStore>();
    }
}
