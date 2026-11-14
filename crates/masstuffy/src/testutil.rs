//! Test helpers.

use std::fs;
use std::num::NonZeroU32;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use masstuffy_cdx::Entry;
use masstuffy_collection::Codec;
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
