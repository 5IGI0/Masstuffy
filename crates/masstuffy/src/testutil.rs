//! Test helpers.

use std::fs;
use std::num::NonZeroU32;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use masstuffy_cdx::Entry;
use masstuffy_collection::{Codec, CollectionId};
use masstuffy_index::{CollectionKey, MAX_LIMIT, SecondaryIndex};
use masstuffy_warc::{Record, RecordBuilder, WarcDate};
use masstuffy_zframe::{DictHeader, SampleSet, TrainOptions, train};

use crate::collection::CollectionHandle;
use crate::config::Config;
use crate::repository::Repository;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A fresh temporary directory removed on drop.
pub(crate) struct TestDir(PathBuf);

impl TestDir {
    pub(crate) fn new() -> TestDir {
        let p = std::env::temp_dir().join(format!(
            "masstuffy-engine-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        TestDir(p)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    /// A not-yet-existing repository directory.
    pub(crate) fn repo(&self) -> PathBuf {
        self.0.join("repo")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Bounds a future so a bug cannot hang the suite.
pub(crate) async fn bounded<F: std::future::Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(60), f)
        .await
        .expect("test timed out")
}

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

pub(crate) fn id_str(i: usize) -> String {
    format!("urn:uuid:00000000-0000-4000-8000-{i:012x}")
}

pub(crate) const DATE: &str = "2026-11-13T10:00:00Z";

pub(crate) fn url(i: usize) -> String {
    format!("https://example.org/{i}")
}

/// A `response` record with an explicit id, date and URL.
pub(crate) fn record(i: usize, body_len: usize) -> Record {
    let mut b = RecordBuilder::new("response").unwrap();
    b.record_id(&id_str(i)).unwrap();
    b.date(WarcDate::parse(DATE).unwrap());
    b.target_uri(&url(i)).unwrap();
    b.build(&letters(i, body_len)).unwrap()
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

pub(crate) fn dict_id() -> u32 {
    DictHeader::parse(base_dict()).unwrap().dict_id
}

pub(crate) fn zstd_codec() -> Codec {
    Codec::Zstd {
        dict_id: NonZeroU32::new(dict_id()).unwrap(),
        level: 1,
    }
}

/// A config with deep queues so group-commit tests can queue many requests.
pub(crate) fn small_config() -> Config {
    Config {
        writer_queue: 256,
        max_group_requests: 256,
        ..Config::default()
    }
}

/// Initialises a repository at `dir` with `config`, installs the test dictionary by writing the
/// file directly, and shuts it down.
pub(crate) async fn prepare_repo(dir: &Path, config: &Config) {
    let repo = Repository::init_with_config(dir, config).await.unwrap();
    repo.shutdown().await.unwrap();
    drop(repo);
    let path = dir
        .join("data")
        .join("dict")
        .join("zstd")
        .join(format!("test.{}.zstdict", dict_id()));
    fs::write(path, base_dict()).unwrap();
}

/// `prepare_repo`, then `open`.
pub(crate) async fn repo_with_dict(td: &TestDir, config: &Config) -> Repository {
    prepare_repo(&td.repo(), config).await;
    Repository::open(td.repo()).await.unwrap().0
}

/// Every live entry of the collection's fsynced CDX, in CDX order (warcinfo included).
pub(crate) async fn live_entries(h: &CollectionHandle) -> Vec<Entry<'static>> {
    let snap = h.shared.published.load();
    let end = h.stats().cdx_len;
    tokio::task::spawn_blocking(move || {
        let mut it = snap.entries(end).unwrap();
        let mut v = Vec::new();
        while let Some((_, e)) = it.next_entry().unwrap() {
            v.push(e.into_owned());
        }
        v
    })
    .await
    .unwrap()
}

type OpenGateState = Arc<(Mutex<bool>, Condvar)>;

static OPEN_GATES: Mutex<Vec<(PathBuf, OpenGateState)>> = Mutex::new(Vec::new());

/// Blocks every `open_one` job under `dir` until [`OpenGate::release`].
pub(crate) struct OpenGate(OpenGateState);

pub(crate) fn hold_open(dir: &Path) -> OpenGate {
    let st: OpenGateState = Arc::new((Mutex::new(false), Condvar::new()));
    OPEN_GATES
        .lock()
        .unwrap()
        .push((dir.to_path_buf(), Arc::clone(&st)));
    OpenGate(st)
}

impl OpenGate {
    pub(crate) fn release(&self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}

/// Called by `open_one` (test builds): waits while a gate covers `path`.
pub(crate) fn open_hook(path: &Path) {
    let st = OPEN_GATES
        .lock()
        .unwrap()
        .iter()
        .find(|(d, _)| path.starts_with(d))
        .map(|(_, s)| Arc::clone(s));
    if let Some(st) = st {
        let g = st.0.lock().unwrap();
        let _ =
            st.1.wait_timeout_while(g, Duration::from_secs(30), |open| !*open)
                .unwrap();
    }
}

static MEMORY_INDEX: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Makes every repository opened under `dir` use the in-memory index (rebuilt at every open).
pub(crate) fn use_memory_index(dir: &Path) {
    MEMORY_INDEX.lock().unwrap().push(dir.to_path_buf());
}

/// Called by `open_backend` (test builds): does a registration cover `path`?
pub(crate) fn memory_index_requested(path: &Path) -> bool {
    MEMORY_INDEX
        .lock()
        .unwrap()
        .iter()
        .any(|d| path.starts_with(d))
}

/// Every visible entry of the collection in `index`, by position (= CDX order).
pub(crate) fn dump_index(index: &dyn SecondaryIndex, id: CollectionId) -> Vec<Entry<'static>> {
    let key = CollectionKey::new(id.as_u128());
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let hits = index.list(key, after, MAX_LIMIT).unwrap();
        let Some(last) = hits.last() else {
            return out;
        };
        after = Some(last.position());
        out.extend(hits.into_iter().map(|h| h.entry));
    }
}

/// `dump_index` of the repository's own index, in `spawn_blocking`.
pub(crate) async fn index_dump(repo: &Repository, id: CollectionId) -> Vec<Entry<'static>> {
    let index = repo.core().index().unwrap();
    tokio::task::spawn_blocking(move || dump_index(&*index, id))
        .await
        .unwrap()
}

pub(crate) fn index_hooks(repo: &Repository) -> Arc<crate::indexer::IndexHooks> {
    Arc::clone(&repo.core().index_hooks)
}

// ---- M19 helpers: envelopes, frames, offline tombstones, scans.

use masstuffy_cdx::{DEFAULT_MAX_LINE, ImportMeta, Kind, digest, encode_entry, encode_header};
use masstuffy_collection::{Collection, RecoveryOptions};
use masstuffy_zframe::{Compressor, EncoderDict};

use crate::scan::{Scan, ScanItem};

/// One envelope item: the claimed metadata and the stored bytes.
pub(crate) type Item = (ImportMeta<'static>, Vec<u8>);

/// Metadata matching `rec` (with the claimed digest of `payload` if `with_digest`).
pub(crate) fn meta_for(rec: &Record, payload: &[u8], with_digest: bool) -> ImportMeta<'static> {
    let v = rec.view();
    ImportMeta {
        warc_type: v.warc_type().to_owned().into(),
        record_id: v.record_id().unwrap().to_owned().into(),
        url: v.target_uri().map(|u| u.to_owned().into()),
        date: v.date().unwrap(),
        raw_size: u64::try_from(payload.len()).unwrap(),
        digest: with_digest.then(|| digest(payload)),
    }
}

/// `(meta_for(record(i)), payload)` for a Codec None collection, with digests.
pub(crate) fn plain_item(i: usize, body_len: usize) -> Item {
    let rec = record(i, body_len);
    let payload = rec.as_bytes().to_vec();
    (meta_for(&rec, &payload, true), payload)
}

/// A `#masstuffy-cdx/1` envelope and the stream offset of each item's line.
pub(crate) fn envelope_v1(items: &[Item]) -> (Vec<u8>, Vec<u64>) {
    let mut v = Vec::new();
    encode_header(0, &mut v);
    let mut offsets = Vec::new();
    for (meta, payload) in items {
        offsets.push(u64::try_from(v.len()).unwrap());
        let e = Entry {
            kind: Kind::Add,
            segment: 0,
            offset: 0,
            raw_size: u64::try_from(payload.len()).unwrap(),
            digest: meta.digest.unwrap_or_else(|| digest(payload)),
            date: meta.date,
            warc_type: meta.warc_type.clone(),
            record_id: meta.record_id.clone(),
            url: meta.url.clone(),
        };
        encode_entry(&e, DEFAULT_MAX_LINE, &mut v).unwrap();
        v.extend_from_slice(payload);
    }
    (v, offsets)
}

/// A legacy envelope (no header, no digest) and the offset of each item's line.
pub(crate) fn envelope_legacy(items: &[Item]) -> (Vec<u8>, Vec<u64>) {
    let date14 = DATE
        .chars()
        .filter(char::is_ascii_digit)
        .collect::<String>();
    let mut v = Vec::new();
    let mut offsets = Vec::new();
    for (meta, payload) in items {
        offsets.push(u64::try_from(v.len()).unwrap());
        let line = format!(
            "{} {} {} {date14} f 0 {}\n",
            meta.url.as_deref().unwrap_or("-"),
            meta.warc_type,
            meta.record_id,
            payload.len()
        );
        v.extend_from_slice(line.as_bytes());
        v.extend_from_slice(payload);
    }
    (v, offsets)
}

/// A copy of [`base_dict`] under another id.
pub(crate) fn other_dict() -> Vec<u8> {
    let mut d = base_dict().to_vec();
    masstuffy_zframe::set_dict_id(&mut d, other_dict_id()).unwrap();
    d
}

pub(crate) fn other_dict_id() -> u32 {
    if dict_id() == 40_000 { 40_001 } else { 40_000 }
}

/// One zstd frame of `src` with `dict`.
pub(crate) fn frame(dict: &[u8], src: &[u8]) -> Vec<u8> {
    let d = EncoderDict::new(dict, 1).unwrap();
    let mut out = Vec::new();
    Compressor::new()
        .unwrap()
        .compress(src, &d, &mut out)
        .unwrap();
    out
}

/// One zstd frame of `src` without dictionary.
pub(crate) fn plain_frame(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    Compressor::new()
        .unwrap()
        .compress_plain(src, 1, &mut out)
        .unwrap();
    out
}

/// A blocking `Read` fed from a channel; EOF when the sender is dropped. Sends the number of
/// bytes consumed so far on `starved` each time it is about to block.
pub(crate) struct ChanReader {
    pub(crate) rx: std::sync::mpsc::Receiver<Vec<u8>>,
    pub(crate) cur: Vec<u8>,
    pub(crate) pos: usize,
    /// Bytes handed out so far.
    pub(crate) consumed: u64,
    pub(crate) starved: Option<std::sync::mpsc::Sender<u64>>,
}

impl std::io::Read for ChanReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.cur.len() {
            if let Some(s) = &self.starved {
                let _ = s.send(self.consumed);
            }
            match self.rx.recv() {
                Ok(v) => {
                    self.cur = v;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.cur.len() - self.pos);
        buf[..n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
        self.pos += n;
        self.consumed += u64::try_from(n).unwrap();
        Ok(n)
    }
}

/// What `offline_tombstone` needs, captured while the repository is up.
pub(crate) struct Offline {
    opts: masstuffy_collection::Options,
    resolver: Arc<crate::resolver::StoreResolver>,
    dir: PathBuf,
}

pub(crate) fn offline_ctx(repo: &Repository, h: &CollectionHandle) -> Offline {
    Offline {
        opts: crate::config::Limits::new(repo.config()).collection_opts,
        resolver: Arc::clone(&h.core.resolver),
        dir: repo.path().join("data/repository").join(h.id().to_string()),
    }
}

/// Appends tombstones for `targets` to the collection's CDX. Blocking; only while the repository
/// is shut down.
pub(crate) fn offline_tombstone(o: &Offline, targets: &[Entry<'static>]) {
    let (mut c, _) =
        Collection::open(&o.dir, &o.opts, &RecoveryOptions::default(), &*o.resolver).unwrap();
    c.tombstone(targets).unwrap();
}

/// The highest-numbered segment file in the collection directory `dir`.
pub(crate) fn last_segment(dir: &Path) -> PathBuf {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("seg."))
        })
        .collect();
    v.sort_by_key(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.split('.').nth(2))
            .and_then(|n| n.parse::<u32>().ok())
    });
    v.pop().unwrap()
}

pub(crate) async fn drain_scan(scan: &mut Scan) -> Vec<ScanItem> {
    let mut v = Vec::new();
    while let Some(p) = scan.next_page().await.unwrap() {
        v.extend(p.items);
    }
    v
}
