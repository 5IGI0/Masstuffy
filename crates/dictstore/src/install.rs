//! Id allocation and `DictStore::install`.

use std::borrow::Cow;
use std::sync::{Arc, PoisonError};

use masstuffy_zframe::{
    DICT_ID_ALLOC_END, DICT_ID_ALLOC_MIN, DecoderDict, DictHeader, is_allocatable_dict_id,
    set_dict_id,
};
use xxhash_rust::xxh3::Xxh3Default;

use crate::error::{Error, Op, io as io_err};
use crate::file::{self, DictInfo};
use crate::name::is_valid_label;
use crate::store::DictStore;

/// Result of [`DictStore::install`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Installed {
    /// The id under which the dictionary is stored (allocatable when `created`).
    pub dict_id: u32,
    /// False if identical content was already in the store (nothing was written).
    pub created: bool,
}

/// Size of the allocatable id range.
// u32 -> u64 widening is lossless; `u64::from` is not callable in a const.
const RANGE: u64 = (DICT_ID_ALLOC_END - DICT_ID_ALLOC_MIN) as u64;

/// xxh3-64 of `bytes` with the id field (bytes `4..8`) read as zeros.
pub(crate) fn content_hash(bytes: &[u8]) -> u64 {
    let mut h = Xxh3Default::new();
    if let Some(head) = bytes.get(..4) {
        h.update(head);
    }
    h.update(&[0; 4]);
    if let Some(tail) = bytes.get(8..) {
        h.update(tail);
    }
    h.digest()
}

/// Picks the id for a dictionary: `trained` if it is allocatable and free, else the first free id
/// found by linear probing in `[DICT_ID_ALLOC_MIN, DICT_ID_ALLOC_END)` from `hash % range`.
/// Pure and deterministic. `taken(id)` says whether an id is in use.
pub(crate) fn choose_id(trained: u32, hash: u64, taken: impl Fn(u32) -> bool) -> Option<u32> {
    if is_allocatable_dict_id(trained) && !taken(trained) {
        return Some(trained);
    }
    let start = hash % RANGE;
    // Terminates after at most `taken` ids + 1 probes in practice; the full range is the bound.
    (0..RANGE).find_map(|i| {
        let off = u32::try_from(start.checked_add(i)? % RANGE).ok()?;
        let cand = DICT_ID_ALLOC_MIN.checked_add(off)?;
        (!taken(cand)).then_some(cand)
    })
}

impl DictStore {
    /// Validates and stores a formatted zstd dictionary; returns its id. Blocking (`fsync`).
    ///
    /// The dictionary keeps its trained id if that id is allocatable (`[32768, 2^31)`) and free;
    /// otherwise a free id is found by deterministic linear probing seeded by an xxh3 hash of the
    /// content (id field zeroed), and the header id is rewritten. The file is written as
    /// `<label>.<id>.zstdict` with `atomic_write`; an existing file is never overwritten (I5).
    ///
    /// Idempotent: if a stored dictionary has identical content (ignoring the id field), nothing is
    /// written and its id is returned with `created: false` (its label is kept, `label` ignored).
    ///
    /// Installs are serialised. The id becomes visible to lookups only once the file is durable. The
    /// engine must persist a manifest referencing the returned id only after `install` returned
    /// `Ok` (I5 ordering).
    ///
    /// # Errors
    ///
    /// [`Error::InvalidLabel`], [`Error::InputTooLarge`], [`Error::InvalidInput`] (not a formatted
    /// dictionary: magic and non-zero id required), [`Error::Codec`] (zstd refuses it),
    /// [`Error::AlreadyExists`], [`Error::NoFreeId`], [`Error::Durable`] (write or `fsync`
    /// failure), and file errors from reading an existing candidate. If the final directory `fsync`
    /// fails after the rename, the file is visible on disk, so it is registered in memory (its id
    /// stays reserved) and the error is returned; retrying the same content then succeeds.
    pub fn install(&self, label: &str, bytes: &[u8]) -> Result<Installed, Error> {
        if !is_valid_label(label) {
            return Err(Error::InvalidLabel {
                label: label.to_owned(),
            });
        }
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let max = self.opts.max_file_size;
        if len > max {
            return Err(Error::InputTooLarge { size: len, max });
        }
        let trained = DictHeader::parse(bytes)
            .map_err(Error::InvalidInput)?
            .dict_id;
        // Full validation by zstd, outside any lock. The result is dropped at once.
        DecoderDict::new(bytes).map_err(|source| Error::Codec {
            dict_id: trained,
            source,
        })?;
        let hash = content_hash(bytes);

        // Held until the end on purpose: installs are rare and must not pick the same id or write
        // the same content twice. Lookups never take this lock.
        let _guard = self
            .install_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);

        // Idempotence: same size, then same bytes except the id field.
        let candidates: Vec<Arc<DictInfo>> = self
            .files_read()
            .values()
            .filter(|i| i.size == len)
            .cloned()
            .collect();
        for info in candidates {
            let existing = file::read_full(&info, max)?;
            if existing.get(..4) == bytes.get(..4) && existing.get(8..) == bytes.get(8..) {
                masstuffy_durable::sync_dir(&self.dir)?;
                return Ok(Installed {
                    dict_id: info.dict_id,
                    created: false,
                });
            }
        }

        let id = {
            let files = self.files_read();
            choose_id(trained, hash, |i| files.contains_key(&i)).ok_or(Error::NoFreeId)?
        };
        let data: Cow<'_, [u8]> = if id == trained {
            Cow::Borrowed(bytes)
        } else {
            let mut owned = bytes.to_vec();
            set_dict_id(&mut owned, id).map_err(Error::InvalidInput)?;
            Cow::Owned(owned)
        };

        let (name, path) = self.path_for(label, id);
        if file::exists_nofollow(&path).map_err(io_err(Op::Stat, &path))? {
            return Err(Error::AlreadyExists { path });
        }
        let info = Arc::new(DictInfo {
            dict_id: id,
            label: label.into(),
            path,
            size: len,
        });
        match masstuffy_durable::atomic_write(&self.dir, &name, &data) {
            Ok(()) => {
                self.files_write().insert(id, info);
                Ok(Installed {
                    dict_id: id,
                    created: true,
                })
            }
            Err(e) => {
                // Renamed but the final directory fsync failed: the file is visible, so its id
                // must not look free (a later install could create a duplicate, I6). If the check
                // itself fails, assume it exists: reserving an id with no file is harmless.
                if file::exists_nofollow(&info.path).unwrap_or(true) {
                    self.files_write().insert(id, info);
                }
                Err(Error::Durable(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Options;
    use crate::name::file_name;
    use crate::testutil::{TestDir, is_root, names, put, trained_id, variant, with_id};
    use masstuffy_zframe::DictError;
    use std::collections::HashSet;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Barrier;
    use std::thread;

    fn open(dir: &std::path::Path) -> DictStore {
        DictStore::open(dir, Options::default()).unwrap().0
    }

    #[test]
    fn choose_id_keeps_trained() {
        assert_eq!(choose_id(40000, 7, |_| false), Some(40000));
    }

    #[test]
    fn choose_id_probes_deterministically() {
        let taken = |i| i == 40000;
        let a = choose_id(40000, 0, taken);
        assert_eq!(a, Some(DICT_ID_ALLOC_MIN));
        assert_eq!(a, choose_id(40000, 0, taken));
        let h = 12345;
        let want = DICT_ID_ALLOC_MIN + u32::try_from(h % RANGE).unwrap();
        assert_eq!(choose_id(40000, h, taken), Some(want));
    }

    #[test]
    fn choose_id_linear_collision() {
        let taken = |i| [40000, 32768, 32769].contains(&i);
        assert_eq!(choose_id(40000, 0, taken), Some(32770));
    }

    #[test]
    fn choose_id_wraps() {
        let taken = |i| i == 40000 || i == DICT_ID_ALLOC_END - 1;
        assert_eq!(choose_id(40000, RANGE - 1, taken), Some(DICT_ID_ALLOC_MIN));
    }

    #[test]
    fn choose_id_non_allocatable_trained() {
        for trained in [5, 1 << 31] {
            let id = choose_id(trained, 99, |_| false).unwrap();
            assert_ne!(id, trained);
            assert!(is_allocatable_dict_id(id));
        }
    }

    #[test]
    fn content_hash_ignores_id() {
        let a = content_hash(&with_id(variant(1), 40000));
        assert_eq!(a, content_hash(&with_id(variant(1), 50000)));
        assert_ne!(a, content_hash(&variant(2)));
    }

    #[test]
    fn install_keeps_trained_id() {
        let t = TestDir::new();
        let s = open(t.path());
        let d = variant(1);
        let t_id = trained_id();
        let r = s.install("lbl", &d).unwrap();
        assert_eq!(
            r,
            Installed {
                dict_id: t_id,
                created: true
            }
        );
        let name = file_name("lbl", t_id);
        assert_eq!(names(t.path()), std::slice::from_ref(&name));
        assert_eq!(fs::read(t.path().join(&name)).unwrap(), d);
        let info = s.get(t_id).unwrap();
        assert_eq!(info.label(), "lbl");
        assert_eq!(info.size(), u64::try_from(d.len()).unwrap());
    }

    fn collision_setup() -> (TestDir, DictStore) {
        let t = TestDir::new();
        put(
            t.path(),
            &file_name("x", trained_id()),
            &with_id(variant(9), trained_id()),
        );
        let s = open(t.path());
        (t, s)
    }

    #[test]
    fn install_collision_rewrites_id() {
        let t_id = trained_id();
        let input = variant(1);
        let want = choose_id(t_id, content_hash(&input), |i| i == t_id).unwrap();
        assert_ne!(want, t_id);
        let mut ids = Vec::new();
        for _ in 0..2 {
            let (t, s) = collision_setup();
            let r = s.install("lbl", &input).unwrap();
            assert!(r.created);
            assert!(is_allocatable_dict_id(r.dict_id));
            assert_eq!(r.dict_id, want);
            let file = fs::read(t.path().join(file_name("lbl", want))).unwrap();
            assert_eq!(DictHeader::parse(&file).unwrap().dict_id, want);
            assert_eq!(file[..4], input[..4]);
            assert_eq!(file[8..], input[8..]);
            assert_eq!(file[4..8], want.to_le_bytes());
            ids.push(r.dict_id);
        }
        assert_eq!(ids[0], ids[1]);
    }

    #[test]
    fn install_idempotent() {
        let t = TestDir::new();
        let s = open(t.path());
        let first = s.install("lbl", &variant(1)).unwrap();
        assert!(first.created);
        let again = s.install("other", &variant(1)).unwrap();
        assert_eq!(
            again,
            Installed {
                dict_id: first.dict_id,
                created: false
            }
        );
        let other_id = s.install("other", &with_id(variant(1), 45000)).unwrap();
        assert_eq!(
            other_id,
            Installed {
                dict_id: first.dict_id,
                created: false
            }
        );
        assert_eq!(names(t.path()).len(), 1);
    }

    #[test]
    fn install_rejects() {
        let t = TestDir::new();
        let s = open(t.path());
        let mut zero = variant(1);
        zero[4..8].copy_from_slice(&[0; 4]);
        let e = s.install("a.b", &variant(1)).err().unwrap();
        assert!(matches!(e, Error::InvalidLabel { .. }), "{e:?}");
        let e = s.install("a", b"not a dict").err().unwrap();
        assert!(
            matches!(e, Error::InvalidInput(DictError::BadMagic(_))),
            "{e:?}"
        );
        let e = s.install("a", &variant(1)[..7]).err().unwrap();
        assert!(
            matches!(e, Error::InvalidInput(DictError::Truncated)),
            "{e:?}"
        );
        let e = s.install("a", &zero).err().unwrap();
        assert!(matches!(e, Error::InvalidInput(DictError::ZeroId)), "{e:?}");
        let e = s.install("a", &variant(1)[..64]).err().unwrap();
        assert!(matches!(e, Error::Codec { .. }), "{e:?}");

        let small = Options {
            max_file_size: 100,
            ..Options::default()
        };
        let (s2, _) = DictStore::open(t.path(), small).unwrap();
        let e = s2.install("a", &variant(1)).err().unwrap();
        assert!(matches!(e, Error::InputTooLarge { max: 100, .. }), "{e:?}");
        assert!(names(t.path()).is_empty());
        assert!(s.list().is_empty());
    }

    #[test]
    fn install_never_overwrites() {
        let t = TestDir::new();
        let s = open(t.path());
        let t_id = trained_id();
        let name = file_name("lbl", t_id);
        put(t.path(), &name, b"hand made");
        let e = s.install("lbl", &variant(1)).err().unwrap();
        assert!(matches!(e, Error::AlreadyExists { .. }), "{e:?}");
        assert_eq!(fs::read(t.path().join(&name)).unwrap(), b"hand made");
        assert!(!s.contains(t_id));
        assert_eq!(names(t.path()), [name]);
    }

    #[test]
    fn install_tmp_failure_leaves_nothing() {
        let t = TestDir::new();
        let s = open(t.path());
        fs::set_permissions(t.path(), fs::Permissions::from_mode(0o555)).unwrap();
        if is_root(t.path()) {
            fs::set_permissions(t.path(), fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("skipped: running as root");
            return;
        }
        let e = s.install("lbl", &variant(1)).err().unwrap();
        fs::set_permissions(t.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(e, Error::Durable(_)), "{e:?}");
        assert!(s.list().is_empty());
        assert!(names(t.path()).is_empty());
    }

    #[test]
    fn install_then_reopen() {
        let t = TestDir::new();
        let s = open(t.path());
        let ids: Vec<u32> = (1..=3)
            .map(|n| s.install(&format!("d{n}"), &variant(n)).unwrap().dict_id)
            .collect();
        assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 3);
        let before: Vec<_> = s.list().iter().map(|i| (i.dict_id(), i.size())).collect();
        drop(s);
        let s = open(t.path());
        let after: Vec<_> = s.list().iter().map(|i| (i.dict_id(), i.size())).collect();
        assert_eq!(before, after);
        for id in ids {
            assert_eq!(s.decoder(id).unwrap().id(), id);
        }
    }

    #[test]
    fn concurrent_installs_distinct() {
        let t = TestDir::new();
        let s = open(t.path());
        let barrier = Barrier::new(8);
        let ids: Vec<u32> = thread::scope(|sc| {
            let hs: Vec<_> = (0..8u32)
                .map(|i| {
                    let (s, barrier) = (&s, &barrier);
                    sc.spawn(move || {
                        barrier.wait();
                        s.install(&format!("t{i}"), &variant(i)).unwrap().dict_id
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 8);
        assert!(ids.iter().all(|i| is_allocatable_dict_id(*i)));
        assert_eq!(ids.iter().filter(|i| **i == trained_id()).count(), 1);
        let n = names(t.path());
        assert_eq!(n.len(), 8);
        assert!(n.iter().all(|f| !f.starts_with(".tmp-")));
        drop(s);
        assert_eq!(open(t.path()).list().len(), 8);
    }

    #[test]
    fn concurrent_installs_same_content() {
        let t = TestDir::new();
        let s = open(t.path());
        let barrier = Barrier::new(8);
        let rs: Vec<Installed> = thread::scope(|sc| {
            let hs: Vec<_> = (0..8)
                .map(|i| {
                    let (s, barrier) = (&s, &barrier);
                    sc.spawn(move || {
                        barrier.wait();
                        s.install(&format!("t{i}"), &variant(1)).unwrap()
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(rs.iter().all(|r| r.dict_id == rs[0].dict_id));
        assert_eq!(rs.iter().filter(|r| r.created).count(), 1);
        assert_eq!(names(t.path()).len(), 1);
    }
}
