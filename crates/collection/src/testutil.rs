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

/// The same record rewritten with a `WARC/1.0` first line.
pub(crate) fn to_v10(r: &Record) -> Record {
    let mut bytes = b"WARC/1.0\r\n".to_vec();
    bytes.extend_from_slice(&r.as_bytes()[10..]);
    Record::from(
        masstuffy_warc::RecordRef::parse(&bytes, &masstuffy_warc::Limits::DEFAULT).unwrap(),
    )
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

// ---- Recovery test helpers.

use std::collections::BTreeSet;
use std::io::BufReader;

use masstuffy_cdx::{CdxReader, CdxWriter, DEFAULT_MAX_LINE, Step, Tail, digest};

use crate::Error;
use crate::prepare::{Prepared, Preparer};
use crate::recover::{RecoveryOptions, RecoveryReport};

/// Copies every regular file of `src` into the new directory `dst` (flat).
pub(crate) fn clone_dir(src: &Path, dst: &Path) {
    fs::create_dir(dst).unwrap();
    for e in fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_file() {
            fs::copy(e.path(), dst.join(e.file_name())).unwrap();
        }
    }
}

/// Sorted (name, content) of every regular file of `dir`.
pub(crate) fn listing(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<(String, Vec<u8>)> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            let e = e.unwrap();
            e.file_type().unwrap().is_file().then(|| {
                (
                    e.file_name().to_string_lossy().into_owned(),
                    fs::read(e.path()).unwrap(),
                )
            })
        })
        .collect();
    v.sort();
    v
}

/// A checksum-valid CDX line: `content`, a space, the 8 hex digits of its check, a newline.
pub(crate) fn with_check(content: &[u8]) -> Vec<u8> {
    let mut v = content.to_vec();
    v.extend_from_slice(format!(" {:08x}\n", digest(content) & 0xffff_ffff).as_bytes());
    v
}

/// Appends one entry to a CDX file.
pub(crate) fn append_cdx_line(path: &Path, e: &Entry<'_>) {
    let file = fs::OpenOptions::new().append(true).open(path).unwrap();
    let mut w = CdxWriter::append(file, DEFAULT_MAX_LINE);
    w.push(e).unwrap();
    w.write_pending().unwrap();
}

/// `(start, end, entry)` of every valid line of `index.<gen>.cdx`.
pub(crate) fn cdx_lines(dir: &Path, generation: u32) -> Vec<(u64, u64, Entry<'static>)> {
    let f = fs::File::open(dir.join(format!("index.{generation}.cdx"))).unwrap();
    let mut r = CdxReader::open(BufReader::new(f), DEFAULT_MAX_LINE).unwrap();
    let mut v = Vec::new();
    loop {
        match r.next_step().unwrap() {
            Step::Entry { offset, entry } => {
                let e = entry.into_owned();
                v.push((offset, r.offset(), e));
            }
            Step::End(_) => return v,
        }
    }
}

/// All entries of `index.<gen>.cdx` and how it ends.
pub(crate) fn read_cdx_all(dir: &Path, generation: u32) -> (Vec<Entry<'static>>, Tail) {
    let f = fs::File::open(dir.join(format!("index.{generation}.cdx"))).unwrap();
    let mut r = CdxReader::open(BufReader::new(f), DEFAULT_MAX_LINE).unwrap();
    let mut v = Vec::new();
    loop {
        match r.next_step().unwrap() {
            Step::Entry { entry, .. } => v.push(entry.into_owned()),
            Step::End(t) => return (v, t),
        }
    }
}

/// `{lo, hi}`, `{b-1, b, b+1}` for each boundary (clamped to `[lo, hi]`) and `lo + k * stride`.
pub(crate) fn offsets(lo: u64, hi: u64, boundaries: &[u64], stride: u64) -> BTreeSet<u64> {
    let mut s = BTreeSet::from([lo, hi]);
    for &b in boundaries {
        for x in [b.saturating_sub(1), b, b.saturating_add(1)] {
            s.insert(x.clamp(lo, hi));
        }
    }
    let mut x = lo;
    while x <= hi {
        s.insert(x);
        x += stride;
    }
    s
}

pub(crate) fn open_with(
    dir: &Path,
    recovery: &RecoveryOptions,
) -> Result<(Collection, RecoveryReport), Error> {
    Collection::open(dir, &Options::default(), recovery, &MapResolver::standard())
}

pub(crate) fn open_ok(dir: &Path) -> (Collection, RecoveryReport) {
    open_with(dir, &RecoveryOptions::default()).unwrap()
}

/// Prepares and commits `recs` as one batch.
pub(crate) fn commit_batch(
    c: &mut Collection,
    p: &mut Preparer,
    recs: Vec<Record>,
) -> Vec<Entry<'static>> {
    let batch: Vec<Prepared> = recs.into_iter().map(|r| p.prepare(r).unwrap()).collect();
    c.writer().commit(&batch).unwrap()
}

/// Path of segment `n` of generation 1.
pub(crate) fn seg1(dir: &Path, zstd: bool, n: u32) -> PathBuf {
    dir.join(format!(
        "seg.1.{n}.{}",
        if zstd { "warc.zst" } else { "warc" }
    ))
}

pub(crate) fn flip_byte(path: &Path, pos: u64) {
    let mut b = fs::read(path).unwrap();
    let i = usize::try_from(pos).unwrap();
    b[i] ^= 0x01;
    fs::write(path, b).unwrap();
}

pub(crate) fn file_len(path: &Path) -> u64 {
    fs::metadata(path).unwrap().len()
}

pub(crate) type Create = fn(&Path) -> (Collection, Entry<'static>);

/// Both codecs.
pub(crate) fn creators() -> [(&'static str, Create); 2] {
    [("none", create_none), ("zstd", create_zstd)]
}

pub(crate) fn truncate(path: &Path, len: u64) {
    let f = fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_len(len).unwrap();
}

// ---- M14 helpers: entries, rebuild.

use masstuffy_cdx::Kind;

use crate::snapshot::{ReadCtx, Snapshot};

/// Drains `Collection::entries()` into owned entries.
pub(crate) fn live(c: &Collection) -> Vec<Entry<'static>> {
    collect(c.entries().unwrap())
}

fn collect(mut it: crate::entries::Entries) -> Vec<Entry<'static>> {
    let mut v = Vec::new();
    while let Some((_, e)) = it.next_entry().unwrap() {
        v.push(e.into_owned());
    }
    v
}

/// Drains `snapshot.entries(end)` into owned entries.
pub(crate) fn drain(snap: &Snapshot, end: u64) -> Vec<Entry<'static>> {
    drain_at(snap, end).into_iter().map(|(_, e)| e).collect()
}

/// Drains `snapshot.entries(end)` into `(cdx offset, entry)`.
pub(crate) fn drain_at(snap: &Snapshot, end: u64) -> Vec<(u64, Entry<'static>)> {
    let mut it = snap.entries(end).unwrap();
    let mut v = Vec::new();
    while let Some((at, e)) = it.next_entry().unwrap() {
        v.push((at, e.into_owned()));
    }
    v
}

/// The serialised record of `e`, read through `snap`.
pub(crate) fn read_bytes(snap: &Snapshot, e: &Entry<'_>) -> Vec<u8> {
    let (mut ctx, mut buf) = (ReadCtx::new(), Vec::new());
    snap.read(e, &mut ctx, &mut buf)
        .unwrap()
        .as_bytes()
        .to_vec()
}

pub(crate) fn none_codec() -> Codec {
    Codec::None
}

/// Zstd with a dictionary other than [`zstd_codec`]'s.
pub(crate) fn other_codec() -> Codec {
    Codec::Zstd {
        dict_id: NonZeroU32::new(other_id()).unwrap(),
        level: 1,
    }
}

/// The fields of `e` that survive a rebuild (the locator does not).
pub(crate) fn identity(e: &Entry<'_>) -> (Kind, String, String, String, Option<String>) {
    (
        e.kind,
        e.warc_type.to_string(),
        e.record_id.to_string(),
        e.date.to_string(),
        e.url.as_ref().map(ToString::to_string),
    )
}

/// Runs `f` on a thread and fails the test if it does not finish within `secs` seconds (a bug
/// must not hang the suite). A panic of `f` is propagated.
pub(crate) fn with_timeout<T: Send + 'static>(
    secs: u64,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    let h = std::thread::spawn(move || {
        let r = f();
        let _ = tx.send(());
        r
    });
    match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
        Ok(()) => h.join().unwrap(),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => match h.join() {
            Err(p) => std::panic::resume_unwind(p),
            Ok(_) => panic!("test thread ended without a result"),
        },
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("test timed out (hang?)"),
    }
}
