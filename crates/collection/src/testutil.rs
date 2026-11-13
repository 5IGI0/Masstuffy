//! Test helpers.

use std::collections::HashMap;
use std::fs;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use masstuffy_cdx::Entry;
use masstuffy_warc::{Record, RecordBuilder};
use masstuffy_zframe::{
    DecoderDict, DictHeader, EncoderDict, SampleSet, TrainOptions, set_dict_id, train,
};

use crate::DEFAULT_SPLIT_THRESHOLD;
use crate::collection::{Collection, CreateSpec};
use crate::context::Options;
use crate::id::CollectionId;
use crate::manifest::{Codec, Generation, Manifest};
use crate::resolve::{DictResolver, ResolveError};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A fresh temporary directory removed on drop.
pub(crate) struct TestDir(PathBuf);

impl TestDir {
    pub(crate) fn new() -> TestDir {
        let p = std::env::temp_dir().join(format!(
            "masstuffy-collection-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        TestDir(p)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    /// A not-yet-existing sub-directory path (for `Collection::create`).
    pub(crate) fn sub(&self) -> PathBuf {
        self.0.join("coll")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Sorted entry names of `dir`.
pub(crate) fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// A small dictionary trained once per test process.
pub(crate) fn base_dict() -> &'static [u8] {
    static D: OnceLock<Vec<u8>> = OnceLock::new();
    D.get_or_init(|| {
        let mut set = SampleSet::new(TrainOptions {
            max_dict_size: 4 * 1024,
            ..TrainOptions::default()
        });
        let mut x: u64 = 3;
        for i in 0..400 {
            let mut s = format!(
                "WARC/1.1\r\nWARC-Type: response\r\nWARC-Target-URI: https://example.org/{i}\r\n\r\n"
            )
            .into_bytes();
            for _ in 0..100 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                s.push(b'a' + u8::try_from(x % 26).unwrap());
            }
            set.push(&s).unwrap();
        }
        train(&set).unwrap()
    })
}

/// The id of [`base_dict`].
pub(crate) fn trained_id() -> u32 {
    DictHeader::parse(base_dict()).unwrap().dict_id
}

/// A valid dictionary with another id than [`trained_id`].
pub(crate) fn other_dict() -> Vec<u8> {
    let mut v = base_dict().to_vec();
    let id = if trained_id() == 40_000 {
        40_001
    } else {
        40_000
    };
    set_dict_id(&mut v, id).unwrap();
    v
}

pub(crate) fn other_id() -> u32 {
    if trained_id() == 40_000 {
        40_001
    } else {
        40_000
    }
}

/// Resolver over an in-memory map; builds the prepared dictionaries on each call.
#[derive(Default)]
pub(crate) struct MapResolver(HashMap<u32, Vec<u8>>);

impl MapResolver {
    pub(crate) fn insert(&mut self, id: u32, bytes: Vec<u8>) {
        self.0.insert(id, bytes);
    }

    /// Knows [`trained_id`] (base dict) and [`other_id`] (other dict).
    pub(crate) fn standard() -> MapResolver {
        let mut r = MapResolver::default();
        r.insert(trained_id(), base_dict().to_vec());
        r.insert(other_id(), other_dict());
        r
    }

    fn bytes(&self, id: NonZeroU32) -> Result<&Vec<u8>, ResolveError> {
        self.0
            .get(&id.get())
            .ok_or(ResolveError::NotFound(id.get()))
    }
}

fn other_err(id: NonZeroU32, e: masstuffy_zframe::CodecError) -> ResolveError {
    ResolveError::Other {
        dict_id: id.get(),
        source: Box::new(e),
    }
}

impl DictResolver for MapResolver {
    fn decoder(&self, id: NonZeroU32) -> Result<Arc<DecoderDict>, ResolveError> {
        DecoderDict::new(self.bytes(id)?)
            .map(Arc::new)
            .map_err(|e| other_err(id, e))
    }

    fn encoder(&self, id: NonZeroU32, level: i32) -> Result<Arc<EncoderDict>, ResolveError> {
        EncoderDict::new(self.bytes(id)?, level)
            .map(Arc::new)
            .map_err(|e| other_err(id, e))
    }
}

pub(crate) fn uuid(n: u128) -> CollectionId {
    CollectionId::from_u128(0x1b4e_28ba_2fa1_41d2_883f_0016_d3cc_0000 + n)
}

pub(crate) fn zstd_codec() -> Codec {
    Codec::Zstd {
        dict_id: NonZeroU32::new(trained_id()).unwrap(),
        level: 1,
    }
}

pub(crate) fn manifest_zstd() -> Manifest {
    Manifest::new(
        uuid(0),
        "test",
        DEFAULT_SPLIT_THRESHOLD,
        Generation {
            id: 1,
            codec: zstd_codec(),
        },
        2,
    )
    .unwrap()
}

pub(crate) fn manifest_none() -> Manifest {
    Manifest::new(
        uuid(0),
        "test",
        DEFAULT_SPLIT_THRESHOLD,
        Generation {
            id: 1,
            codec: Codec::None,
        },
        2,
    )
    .unwrap()
}

/// Pseudo-random lowercase letters seeded by `seed`.
pub(crate) fn letters(seed: usize, n: usize) -> Vec<u8> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15 ^ u64::try_from(seed).unwrap().wrapping_mul(0x1000_0001);
    if x == 0 {
        x = 1;
    }
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            b'a' + u8::try_from(x % 26).unwrap()
        })
        .collect()
}

/// A `response` record with id `<urn:uuid:00000000-0000-4000-8000-{i:012x}>`.
pub(crate) fn record(i: usize, body_len: usize) -> Record {
    let mut b = RecordBuilder::new("response").unwrap();
    b.record_id(&format!("urn:uuid:00000000-0000-4000-8000-{i:012x}"))
        .unwrap();
    b.date(masstuffy_warc::WarcDate::parse("2026-11-13T10:00:00Z").unwrap());
    b.target_uri(&format!("https://example.org/{i}")).unwrap();
    b.build(&letters(i, body_len)).unwrap()
}

/// A record whose serialised size is exactly `total` bytes.
pub(crate) fn record_of_size(i: usize, total: usize) -> Record {
    let base = record(i, 0).as_bytes().len();
    let mut body = total - base;
    loop {
        let r = record(i, body);
        let len = r.as_bytes().len();
        if len == total {
            return r;
        }
        body = body.checked_sub(len.saturating_sub(total)).unwrap();
        if len < total {
            body += total - len;
        }
    }
}

pub(crate) fn id_str(i: usize) -> String {
    format!("urn:uuid:00000000-0000-4000-8000-{i:012x}")
}

pub(crate) fn create_with(
    dir: &Path,
    codec: Codec,
    threshold: NonZeroU64,
    opts: &Options,
) -> (Collection, Entry<'static>) {
    Collection::create(
        dir,
        CreateSpec {
            uuid: uuid(0),
            slug: "test",
            codec,
            split_threshold: threshold,
        },
        opts,
        &MapResolver::standard(),
    )
    .unwrap()
}

pub(crate) fn create_none(dir: &Path) -> (Collection, Entry<'static>) {
    create_with(
        dir,
        Codec::None,
        DEFAULT_SPLIT_THRESHOLD,
        &Options::default(),
    )
}

pub(crate) fn create_zstd(dir: &Path) -> (Collection, Entry<'static>) {
    create_with(
        dir,
        zstd_codec(),
        DEFAULT_SPLIT_THRESHOLD,
        &Options::default(),
    )
}
