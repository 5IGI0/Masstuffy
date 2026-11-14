//! Engine tests: lifecycle, startup rules, writes and reads, raw import, shutdown.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use masstuffy_cdx::{ImportMeta, Kind};
use masstuffy_collection::{Codec, CollectionId, PrepareError, RawCheck, RawEncoding, ReadError};
use masstuffy_warc::{Limits, Record, RecordRef, WarcDate};
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::error::{Error, ErrorKind};
use crate::repository::Repository;
use crate::testutil::*;
use crate::types::{Located, RawItem, SkipReason};

macro_rules! rt_test {
    ($(#[$m:meta])* async fn $name:ident() $body:block) => {
        $(#[$m])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            bounded(async $body).await
        }
    };
}

fn coll_dir(repo: &Repository, id: CollectionId) -> PathBuf {
    repo.path().join("data/repository").join(id.to_string())
}

async fn open_with(dir: &Path, check: RawCheck) -> Repository {
    Repository::open_inner(dir.to_path_buf(), check)
        .await
        .unwrap()
        .0
}

fn copy_flat(src: &Path, dst: &Path) {
    fs::create_dir(dst).unwrap();
    for e in fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_file() {
            fs::copy(e.path(), dst.join(e.file_name())).unwrap();
        }
    }
}

fn meta_of(i: usize, len: usize) -> ImportMeta<'static> {
    ImportMeta {
        warc_type: "response".into(),
        record_id: id_str(i).into(),
        url: Some(url(i).into()),
        date: WarcDate::parse(DATE).unwrap(),
        raw_size: u64::try_from(len).unwrap(),
        digest: None,
    }
}

fn bytes_of(records: &[Record]) -> Vec<Vec<u8>> {
    records.iter().map(|r| r.as_bytes().to_vec()).collect()
}

// ---- Lifecycle.

rt_test! {
async fn init_creates_layout() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let d = td.repo();
    assert!(d.join("config.json").is_file());
    assert!(d.join(".lock").is_file());
    for p in ["data", "data/buffer", "data/dict", "data/dict/zstd", "data/repository"] {
        assert!(d.join(p).is_dir(), "{p}");
    }
    assert!(repo.collections().is_empty());
    assert_eq!(*repo.config(), Config::default());
    let on_disk = crate::config::load(&d).unwrap();
    assert_eq!(on_disk, Config::default());
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn init_on_non_empty_dir_fails() {
    let td = TestDir::new();
    fs::write(td.path().join("foo.txt"), b"x").unwrap();
    let e = Repository::init(td.path()).await.err().unwrap();
    assert!(matches!(e, Error::NotEmpty { .. }), "{e:?}");
    assert!(!td.path().join("config.json").exists());

    let td2 = TestDir::new();
    fs::create_dir_all(td2.path().join("data")).unwrap();
    fs::write(td2.path().join("data/x"), b"x").unwrap();
    let e = Repository::init(td2.path()).await.err().unwrap();
    assert!(matches!(e, Error::NotEmpty { .. }), "{e:?}");
    assert!(!td2.path().join("config.json").exists());
}
}

rt_test! {
async fn init_on_existing_repo_fails() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    repo.shutdown().await.unwrap();
    let e = Repository::init(td.repo()).await.err().unwrap();
    assert!(matches!(e, Error::AlreadyInitialized { .. }), "{e:?}");
}
}

rt_test! {
async fn init_resumes_interrupted_init() {
    let td = TestDir::new();
    let d = td.repo();
    fs::create_dir_all(d.join("data/dict")).unwrap();
    fs::write(d.join(".lock"), b"").unwrap();
    fs::write(d.join(".tmp-1-2"), b"junk").unwrap();
    let repo = Repository::init(&d).await.unwrap();
    assert!(!d.join(".tmp-1-2").exists());
    for p in ["data/buffer", "data/dict/zstd", "data/repository"] {
        assert!(d.join(p).is_dir(), "{p}");
    }
    assert!(d.join("config.json").is_file());
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn open_without_config_fails() {
    let td = TestDir::new();
    let e = Repository::open(td.path()).await.err().unwrap();
    assert!(matches!(e, Error::NotARepository { .. }), "{e:?}");
    fs::create_dir_all(td.path().join("data/repository")).unwrap();
    let e = Repository::open(td.path()).await.err().unwrap();
    assert!(matches!(e, Error::NotARepository { .. }), "{e:?}");
}
}

rt_test! {
async fn open_missing_layout_fails() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    repo.shutdown().await.unwrap();
    drop(repo);
    fs::remove_dir(td.repo().join("data/repository")).unwrap();
    let e = Repository::open(td.repo()).await.err().unwrap();
    assert!(matches!(e, Error::Layout { .. }), "{e:?}");
    assert_eq!(e.kind(), ErrorKind::Corrupt);
}
}

rt_test! {
async fn second_open_is_locked() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let e = Repository::open(td.repo()).await.err().unwrap();
    assert!(matches!(e, Error::Locked { .. }), "{e:?}");
    assert_eq!(e.kind(), ErrorKind::Conflict);
    repo.shutdown().await.unwrap();
    drop(repo);
    Repository::open(td.repo()).await.unwrap();
}
}

rt_test! {
async fn shutdown_releases_lock_even_with_clones_dropped_later() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    repo.shutdown().await.unwrap();
    // `repo` and `h` are still alive.
    let (again, _) = Repository::open(td.repo()).await.unwrap();
    again.shutdown().await.unwrap();
    drop(h);
}
}

rt_test! {
async fn drop_without_shutdown_releases_lock() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    let at = h.put(vec![record(1, 50)]).await.unwrap();
    let expect = record(1, 50).as_bytes().to_vec();
    drop(h);
    drop(repo);
    let mut opened = None;
    for _ in 0..5000 {
        match Repository::open(td.repo()).await {
            Ok((r, _)) => {
                opened = Some(r);
                break;
            }
            Err(Error::Locked { .. }) => tokio::time::sleep(std::time::Duration::from_millis(1)).await,
            Err(e) => panic!("{e:?}"),
        }
    }
    let repo = opened.expect("lock never released");
    assert_eq!(repo.get(&at[0]).await.unwrap(), expect);
    repo.shutdown().await.unwrap();
}
}

// ---- Startup rules.

rt_test! {
async fn invalid_collection_is_skipped_with_report() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let a = repo.create_collection("a", Codec::None).await.unwrap();
    let b = repo.create_collection("b", Codec::None).await.unwrap();
    a.put(vec![record(1, 10)]).await.unwrap();
    let lb = b.put(vec![record(2, 10)]).await.unwrap();
    let a_dir = coll_dir(&repo, a.id());
    repo.shutdown().await.unwrap();
    drop((repo, a, b));
    fs::write(a_dir.join("manifest.json"), b"{}").unwrap();
    let (repo, report) = Repository::open(td.repo()).await.unwrap();
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].dir, a_dir);
    assert!(matches!(report.skipped[0].reason, SkipReason::Open(_)));
    assert_eq!(report.loaded.len(), 1);
    assert_eq!(&*report.loaded[0].slug, "b");
    repo.collection("b").unwrap();
    assert_eq!(repo.get(&lb[0]).await.unwrap(), record(2, 10).as_bytes());
    assert!(matches!(repo.collection("a").err().unwrap(), Error::UnknownCollection(_)));
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn missing_dictionary_skips_and_reserves_slug() {
    let td = TestDir::new();
    let repo = repo_with_dict(&td, &Config::default()).await;
    repo.create_collection("z", zstd_codec()).await.unwrap();
    repo.shutdown().await.unwrap();
    drop(repo);
    fs::remove_file(
        td.repo().join("data/dict/zstd").join(format!("test.{}.zstdict", dict_id())),
    )
    .unwrap();
    let (repo, report) = Repository::open(td.repo()).await.unwrap();
    assert_eq!(report.skipped.len(), 1);
    assert!(matches!(
        report.skipped[0].reason,
        SkipReason::Open(masstuffy_collection::Error::Resolve(_))
    ));
    let e = repo.create_collection("z", Codec::None).await.err().unwrap();
    assert!(matches!(e, Error::SlugTaken { .. }), "{e:?}");
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn aborted_create_is_trashed() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    repo.shutdown().await.unwrap();
    drop(repo);
    let id = CollectionId::from_u128(0x1234_5678);
    let dir = td.repo().join("data/repository").join(id.to_string());
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("seg.1.1.warc"), b"x").unwrap();
    let (repo, report) = Repository::open(td.repo()).await.unwrap();
    assert_eq!(report.trashed, vec![dir.clone()]);
    assert!(!dir.exists());
    let left: Vec<_> = fs::read_dir(td.repo().join("data/repository")).unwrap().collect();
    assert!(left.is_empty());
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn unknown_entries_kept() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    repo.shutdown().await.unwrap();
    drop(repo);
    let r = td.repo().join("data/repository");
    fs::create_dir(r.join("notes")).unwrap();
    fs::write(r.join("file.txt"), b"x").unwrap();
    fs::create_dir(r.join(".trash-x-1-1")).unwrap();
    let (repo, report) = Repository::open(td.repo()).await.unwrap();
    assert_eq!(report.unknown_count, 2);
    let names: BTreeSet<_> = report
        .unknown
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, BTreeSet::from(["notes".to_owned(), "file.txt".to_owned()]));
    assert!(r.join("notes").is_dir() && r.join("file.txt").is_file());
    assert_eq!(report.trash_removed, 1);
    assert!(!r.join(".trash-x-1-1").exists());
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn uuid_mismatch_skipped() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let a = repo.create_collection("a", Codec::None).await.unwrap();
    let src = coll_dir(&repo, a.id());
    repo.shutdown().await.unwrap();
    drop((repo, a));
    let other = td.repo().join("data/repository").join(CollectionId::from_u128(77).to_string());
    copy_flat(&src, &other);
    let (repo, report) = Repository::open(td.repo()).await.unwrap();
    assert_eq!(report.loaded.len(), 1);
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].dir, other);
    assert!(matches!(report.skipped[0].reason, SkipReason::UuidMismatch { .. }));
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn duplicate_slug_skips_all() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let x = repo.create_collection("x", Codec::None).await.unwrap();
    let src = coll_dir(&repo, x.id());
    repo.shutdown().await.unwrap();
    drop((repo, x));
    let new_id = CollectionId::from_u128(99);
    let other = td.repo().join("data/repository").join(new_id.to_string());
    copy_flat(&src, &other);
    let mut v: serde_json::Value =
        serde_json::from_slice(&fs::read(other.join("manifest.json")).unwrap()).unwrap();
    v["uuid"] = serde_json::Value::String(new_id.to_string());
    fs::write(other.join("manifest.json"), serde_json::to_vec(&v).unwrap()).unwrap();
    let (repo, report) = Repository::open(td.repo()).await.unwrap();
    assert!(report.loaded.is_empty());
    assert_eq!(report.skipped.len(), 2);
    assert!(report
        .skipped
        .iter()
        .all(|s| matches!(s.reason, SkipReason::DuplicateSlug { .. })));
    assert!(repo.collections().is_empty());
    let e = repo.create_collection("x", Codec::None).await.err().unwrap();
    assert!(matches!(e, Error::SlugTaken { .. }), "{e:?}");
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn open_parallel_many_collections() {
    let td = TestDir::new();
    let cfg = Config { open_concurrency: 3, ..Config::default() };
    let repo = Repository::init_with_config(td.repo(), &cfg).await.unwrap();
    let mut saved = Vec::new();
    for c in 0..10usize {
        let h = repo.create_collection(&format!("c{c}"), Codec::None).await.unwrap();
        let recs: Vec<Record> = (0..3).map(|k| record(c * 10 + k, 20)).collect();
        let bytes = bytes_of(&recs);
        let at = h.put(recs).await.unwrap();
        saved.extend(at.into_iter().zip(bytes));
    }
    repo.shutdown().await.unwrap();
    drop(repo);
    let (repo, report) = Repository::open(td.repo()).await.unwrap();
    assert_eq!(report.loaded.len(), 10);
    assert_eq!(repo.collections().len(), 10);
    for (at, bytes) in &saved {
        assert_eq!(&repo.get(at).await.unwrap(), bytes);
    }
    repo.shutdown().await.unwrap();
}
}

// ---- Writes and reads.

async fn round_trip(codec: Codec, with_dict: bool) {
    let td = TestDir::new();
    let repo = if with_dict {
        repo_with_dict(&td, &Config::default()).await
    } else {
        Repository::init(td.repo()).await.unwrap()
    };
    let h = repo.create_collection("rt", codec).await.unwrap();
    let mut recs: Vec<Record> = (0..4).map(|i| record(i, 300)).collect();
    if with_dict {
        recs.push(record(4, 2 << 20));
    } else {
        recs.push(record(4, 3000));
    }
    let bytes = bytes_of(&recs);
    let at = h.put(recs).await.unwrap();
    assert_eq!(at.len(), 5);
    for (l, b) in at.iter().zip(&bytes) {
        assert_eq!(&repo.get(l).await.unwrap(), b);
        let raw = repo.get_raw(l).await.unwrap();
        match codec {
            Codec::None => {
                assert_eq!(raw.encoding, RawEncoding::Warc);
                assert_eq!(&raw.bytes, b);
            }
            Codec::Zstd { dict_id, .. } => {
                let expect = if b.len() > (1 << 20) {
                    0
                } else {
                    dict_id.get()
                };
                assert_eq!(raw.encoding, RawEncoding::Zstd { dict_id: expect });
            }
        }
    }
    repo.shutdown().await.unwrap();
}

rt_test! {
async fn put_get_round_trip_none() {
    round_trip(Codec::None, false).await;
}
}

rt_test! {
async fn put_get_round_trip_zstd() {
    round_trip(zstd_codec(), true).await;
}
}

rt_test! {
async fn put_fills_missing_headers() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let h = repo.create_collection("f", Codec::None).await.unwrap();
    // The builder insists on both headers: strip them from a built record's bytes instead.
    let full = record(1, 5);
    let text = String::from_utf8(full.as_bytes().to_vec()).unwrap();
    let stripped: String = text
        .split_inclusive("\r\n")
        .filter(|l| !l.starts_with("WARC-Record-ID:") && !l.starts_with("WARC-Date:"))
        .collect();
    let view = RecordRef::parse(stripped.as_bytes(), &Limits::DEFAULT).unwrap();
    assert!(view.record_id().is_none() && view.date().is_none());
    let rec = Record::from(view);
    let at = h.put(vec![rec]).await.unwrap();
    assert!(at[0].entry.record_id.starts_with("urn:uuid:"));
    let bytes = repo.get(&at[0]).await.unwrap();
    let view = RecordRef::parse(&bytes, &Limits::DEFAULT).unwrap();
    assert_eq!(view.record_id(), Some(&*at[0].entry.record_id));
    assert!(view.date().is_some());
    assert_eq!(view.body().len(), 5);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn put_empty_is_noop() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let h = repo.create_collection("e", Codec::None).await.unwrap();
    let before = h.stats();
    assert!(h.put(Vec::new()).await.unwrap().is_empty());
    assert!(h.put_raw(Vec::new()).await.unwrap().is_empty());
    assert_eq!(h.stats(), before);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn put_prepare_error_writes_nothing() {
    let td = TestDir::new();
    let cfg = Config { max_record_bytes: 4096, ..Config::default() };
    let repo = Repository::init_with_config(td.repo(), &cfg).await.unwrap();
    let h = repo.create_collection("p", Codec::None).await.unwrap();
    let e = h
        .put(vec![record(1, 10), record(2, 5000), record(3, 10)])
        .await
        .err()
        .unwrap();
    assert!(matches!(e, Error::Prepare { index: 1, .. }), "{e:?}");
    assert_eq!(e.kind(), ErrorKind::Invalid);
    assert_eq!(live_entries(&h).await.len(), 1);
    repo.shutdown().await.unwrap();
}
}

async fn concurrent_puts(codec: Codec, with_dict: bool) {
    const TASKS: usize = 16;
    const REQS: usize = 25;
    let td = TestDir::new();
    let repo = if with_dict {
        repo_with_dict(&td, &small_config()).await
    } else {
        Repository::init_with_config(td.repo(), &small_config())
            .await
            .unwrap()
    };
    let h = repo.create_collection("c", codec).await.unwrap();
    let mut joins = Vec::new();
    for t in 0..TASKS {
        let h = h.clone();
        joins.push(tokio::spawn(async move {
            let mut out = Vec::new();
            for j in 0..REQS {
                let n = 1 + (t + j) % 4;
                let recs: Vec<Record> = (0..n)
                    .map(|k| record(t * 10_000 + j * 100 + k, 60))
                    .collect();
                let bytes = bytes_of(&recs);
                let at = h.put(recs).await.unwrap();
                out.push((at, bytes));
            }
            out
        }));
    }
    let mut results = Vec::new();
    for j in joins {
        results.extend(j.await.unwrap());
    }
    let total: usize = results.iter().map(|(a, _)| a.len()).sum();

    // I9: each reply is contiguous in one segment.
    for (at, _) in &results {
        for w in at.windows(2) {
            assert_eq!(w[0].entry.segment, w[1].entry.segment);
            assert_eq!(w[1].entry.offset, w[0].entry.offset + w[0].entry.raw_size);
        }
    }
    // Every record exactly once, each request consecutive in CDX order.
    let entries = live_entries(&h).await;
    assert_eq!(entries.len(), 1 + total);
    let mut pos: HashMap<String, usize> = HashMap::new();
    for (p, e) in entries.iter().enumerate() {
        assert!(
            pos.insert(e.record_id.to_string(), p).is_none(),
            "duplicate {}",
            e.record_id
        );
    }
    for (at, _) in &results {
        let p0 = pos[&*at[0].entry.record_id];
        for (k, l) in at.iter().enumerate() {
            assert_eq!(pos[&*l.entry.record_id], p0 + k, "interleaved request");
        }
    }
    for (i, (at, bytes)) in results.iter().enumerate().step_by(8) {
        let k = i % at.len();
        assert_eq!(repo.get(&at[k]).await.unwrap(), bytes[k]);
    }
    let st = h.stats();
    assert_eq!(st.records, u64::try_from(total).unwrap());
    assert!(st.commits <= u64::try_from(TASKS * REQS).unwrap());
    repo.shutdown().await.unwrap();
}

rt_test! {
async fn concurrent_puts_all_present_once_contiguous_none() {
    concurrent_puts(Codec::None, false).await;
}
}

rt_test! {
async fn concurrent_puts_all_present_once_contiguous_zstd() {
    concurrent_puts(zstd_codec(), true).await;
}
}

fn arm(h: &crate::collection::CollectionHandle) -> Arc<Semaphore> {
    let s = Arc::new(Semaphore::new(0));
    *h.shared.hooks.hold.lock().unwrap() = Some(Arc::clone(&s));
    s
}

rt_test! {
async fn group_commit_groups() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &small_config()).await.unwrap();
    let h = repo.create_collection("g", Codec::None).await.unwrap();
    let hold = arm(&h);
    let before = h.stats().commits;
    let mut rxs = Vec::new();
    for i in 0..32 {
        rxs.push(h.submit_records(vec![record(i, 40)]).await);
    }
    hold.add_permits(1);
    let mut ids = Vec::new();
    for rx in rxs {
        let at = h.finish(rx).await.unwrap();
        assert_eq!(at.len(), 1);
        ids.push(at[0].entry.record_id.to_string());
    }
    assert_eq!(h.stats().commits - before, 1);
    let entries = live_entries(&h).await;
    let got: Vec<String> = entries.iter().skip(1).map(|e| e.record_id.to_string()).collect();
    let want: Vec<String> = (0..32).map(id_str).collect();
    assert_eq!(got, want);
    assert_eq!(ids, want);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn group_respects_byte_cap() {
    let td = TestDir::new();
    let cfg = Config { max_group_bytes: 64 * 1024, ..small_config() };
    let repo = Repository::init_with_config(td.repo(), &cfg).await.unwrap();
    let h = repo.create_collection("g", Codec::None).await.unwrap();
    let hold = arm(&h);
    let before = h.stats().commits;
    let mut rxs = Vec::new();
    for i in 0..8 {
        rxs.push(h.submit_records(vec![record(i, 20_000)]).await);
    }
    hold.add_permits(100);
    let mut lens = Vec::new();
    for rx in rxs {
        let at = h.finish(rx).await.unwrap();
        lens.push(at[0].entry.raw_size);
    }
    // The drain rule: the first request always joins; the next ones join while the sum stays
    // within the cap, else they start the next group.
    let cap = 64 * 1024u64;
    let mut expected = 0u64;
    let mut sum = 0u64;
    for (i, l) in lens.iter().enumerate() {
        if i == 0 || sum + l > cap {
            expected += 1;
            sum = *l;
        } else {
            sum += l;
        }
    }
    assert!(expected >= 2);
    assert_eq!(h.stats().commits - before, expected);

    let before = h.stats().commits;
    let big = h.put(vec![record(100, 200 * 1024)]).await.unwrap();
    assert_eq!(h.stats().commits - before, 1);
    assert_eq!(repo.get(&big[0]).await.unwrap(), record(100, 200 * 1024).as_bytes());
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn concurrent_reads_while_writing() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &small_config()).await.unwrap();
    let h = repo.create_collection("r", Codec::None).await.unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<(Located, Vec<u8>)>();
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    let mut writers = Vec::new();
    for t in 0..8usize {
        let h = h.clone();
        let tx = tx.clone();
        writers.push(tokio::spawn(async move {
            for j in 0..10usize {
                let recs: Vec<Record> =
                    (0..2).map(|k| record(t * 1000 + j * 10 + k, 100)).collect();
                let bytes = bytes_of(&recs);
                let at = h.put(recs).await.unwrap();
                for pair in at.into_iter().zip(bytes) {
                    tx.send(pair).unwrap();
                }
            }
        }));
    }
    drop(tx);
    let mut readers = Vec::new();
    for _ in 0..8 {
        let repo = repo.clone();
        let rx = Arc::clone(&rx);
        readers.push(tokio::spawn(async move {
            let mut n = 0usize;
            loop {
                let item = rx.lock().await.recv().await;
                let Some((at, bytes)) = item else { break };
                assert_eq!(repo.get(&at).await.unwrap(), bytes);
                n += 1;
            }
            n
        }));
    }
    for w in writers {
        w.await.unwrap();
    }
    let mut read = 0;
    for r in readers {
        read += r.await.unwrap();
    }
    assert_eq!(read, 8 * 10 * 2);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn restart_reads_everything() {
    let td = TestDir::new();
    let repo = repo_with_dict(&td, &small_config()).await;
    let h = repo.create_collection("s", zstd_codec()).await.unwrap();
    let mut saved = Vec::new();
    for j in 0..20usize {
        let recs: Vec<Record> = (0..10).map(|k| record(j * 10 + k, 200)).collect();
        let bytes = bytes_of(&recs);
        saved.extend(h.put(recs).await.unwrap().into_iter().zip(bytes));
    }
    repo.shutdown().await.unwrap();
    drop((repo, h));
    let (repo, report) = Repository::open(td.repo()).await.unwrap();
    assert_eq!(report.loaded.len(), 1);
    assert_eq!(report.loaded[0].recovery.entries, 201);
    for (at, b) in &saved {
        assert_eq!(&repo.get(at).await.unwrap(), b);
    }
    let h = repo.collection("s").unwrap();
    let at = h.put(vec![record(5000, 50)]).await.unwrap();
    assert_eq!(repo.get(&at[0]).await.unwrap(), record(5000, 50).as_bytes());
    assert_eq!(repo.get(&saved[0].0).await.unwrap(), saved[0].1);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn get_stale_locator() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let h = repo.create_collection("s", Codec::None).await.unwrap();
    let at = h.put(vec![record(1, 10)]).await.unwrap().remove(0);
    let mut stale = at.clone();
    stale.generation += 1;
    let e = repo.get(&stale).await.err().unwrap();
    assert!(matches!(e, Error::StaleLocator { .. }), "{e:?}");
    assert_eq!(e.kind(), ErrorKind::Conflict);
    let mut unknown = at.clone();
    unknown.collection = CollectionId::from_u128(5);
    let e = repo.get(&unknown).await.err().unwrap();
    assert!(matches!(e, Error::UnknownCollection(_)), "{e:?}");
    let mut tomb = at.clone();
    tomb.entry.kind = Kind::Tombstone;
    let e = repo.get(&tomb).await.err().unwrap();
    assert!(matches!(e, Error::Read { source: ReadError::Tombstone, .. }), "{e:?}");
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn get_corrupt_digest() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let h = repo.create_collection("s", Codec::None).await.unwrap();
    let mut at = h.put(vec![record(1, 10)]).await.unwrap().remove(0);
    at.entry.digest ^= 1;
    let e = repo.get(&at).await.err().unwrap();
    assert!(
        matches!(e, Error::Read { source: ReadError::DigestMismatch { .. }, .. }),
        "{e:?}"
    );
    assert_eq!(e.kind(), ErrorKind::Corrupt);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn writer_reopens_after_loss() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let h = repo.create_collection("w", Codec::None).await.unwrap();
    let a = h.put(vec![record(1, 10)]).await.unwrap();
    h.shared.hooks.drop_writer_next.store(true, Ordering::Release);
    let b = h.put(vec![record(2, 10)]).await.unwrap();
    assert!(!h.shared.hooks.drop_writer_next.load(Ordering::Acquire));
    let c = h.put(vec![record(3, 10)]).await.unwrap();
    repo.shutdown().await.unwrap();
    drop((repo, h));
    let (repo, report) = Repository::open(td.repo()).await.unwrap();
    assert_eq!(report.loaded[0].recovery.entries, 4);
    for (l, i) in [(&a[0], 1), (&b[0], 2), (&c[0], 3)] {
        assert_eq!(repo.get(l).await.unwrap(), record(i, 10).as_bytes());
    }
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn create_collection_validation() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let e = repo.create_collection("Bad Slug", Codec::None).await.err().unwrap();
    assert!(matches!(e, Error::InvalidSlug { .. }), "{e:?}");
    let e = repo.create_collection("z", zstd_codec()).await.err().unwrap();
    assert!(matches!(e, Error::DictNotFound(_)), "{e:?}");
    repo.create_collection("ok", Codec::None).await.unwrap();
    let e = repo.create_collection("ok", Codec::None).await.err().unwrap();
    assert!(matches!(e, Error::SlugTaken { .. }), "{e:?}");
    assert_eq!(repo.collections().len(), 1);
    assert_eq!(repo.collection("ok").unwrap().info().generation, 1);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn concurrent_creates_same_slug() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let mut joins = Vec::new();
    for _ in 0..8 {
        let repo = repo.clone();
        joins.push(tokio::spawn(async move {
            repo.create_collection("same", Codec::None).await.map(|_| ())
        }));
    }
    let mut ok = 0;
    for j in joins {
        match j.await.unwrap() {
            Ok(()) => ok += 1,
            Err(Error::SlugTaken { .. }) => {}
            Err(e) => panic!("{e:?}"),
        }
    }
    assert_eq!(ok, 1);
    let dirs = fs::read_dir(td.repo().join("data/repository")).unwrap().count();
    assert_eq!(dirs, 1);
    repo.shutdown().await.unwrap();
}
}

// ---- Raw import.

rt_test! {
async fn put_raw_full_none() {
    let td = TestDir::new();
    prepare_repo(&td.repo(), &Config::default()).await;
    let repo = open_with(&td.repo(), RawCheck::Full).await;
    let h = repo.create_collection("raw", Codec::None).await.unwrap();
    let rec = record(1, 100);
    let payload = rec.as_bytes().to_vec();
    let at = h
        .put_raw(vec![RawItem { meta: meta_of(1, payload.len()), payload: payload.clone() }])
        .await
        .unwrap();
    assert_eq!(repo.get(&at[0]).await.unwrap(), payload);

    let before = live_entries(&h).await.len();
    let e = h
        .put_raw(vec![RawItem { meta: meta_of(2, payload.len()), payload }])
        .await
        .err()
        .unwrap();
    assert!(
        matches!(e, Error::Prepare { index: 0, source: PrepareError::MetaMismatch(_) }),
        "{e:?}"
    );
    assert_eq!(live_entries(&h).await.len(), before);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn put_raw_header_only_mismatch_readable_raw_only() {
    let td = TestDir::new();
    prepare_repo(&td.repo(), &Config::default()).await;
    let repo = open_with(&td.repo(), RawCheck::HeaderOnly).await;
    assert_eq!(repo.raw_check(), RawCheck::HeaderOnly);
    let h = repo.create_collection("raw", Codec::None).await.unwrap();
    let payload = record(1, 100).as_bytes().to_vec();
    let at = h
        .put_raw(vec![RawItem { meta: meta_of(2, payload.len()), payload: payload.clone() }])
        .await
        .unwrap();
    let e = repo.get(&at[0]).await.err().unwrap();
    assert!(matches!(e, Error::NotDecodable { .. }), "{e:?}");
    let raw = repo.get_raw(&at[0]).await.unwrap();
    assert_eq!(raw.bytes, payload);
    assert_eq!(raw.encoding, RawEncoding::Warc);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn put_raw_zstd_round_trip() {
    let td = TestDir::new();
    prepare_repo(&td.repo(), &Config::default()).await;
    let repo = open_with(&td.repo(), RawCheck::Full).await;
    let src = repo.create_collection("src", zstd_codec()).await.unwrap();
    let dst = repo.create_collection("dst", zstd_codec()).await.unwrap();
    let none = repo.create_collection("none", Codec::None).await.unwrap();
    let rec = record(1, 500);
    let at = src.put(vec![rec.clone()]).await.unwrap().remove(0);
    let raw = repo.get_raw(&at).await.unwrap();
    let item = |raw: &crate::types::RawRecord| RawItem {
        meta: ImportMeta {
            warc_type: at.entry.warc_type.to_string().into(),
            record_id: at.entry.record_id.to_string().into(),
            url: at.entry.url.as_ref().map(|u| u.to_string().into()),
            date: at.entry.date,
            raw_size: at.entry.raw_size,
            digest: Some(at.entry.digest),
        },
        payload: raw.bytes.clone(),
    };
    let moved = dst.put_raw(vec![item(&raw)]).await.unwrap();
    assert_eq!(repo.get(&moved[0]).await.unwrap(), rec.as_bytes());
    let e = none.put_raw(vec![item(&raw)]).await.err().unwrap();
    assert!(matches!(e, Error::Prepare { .. }), "{e:?}");
    repo.shutdown().await.unwrap();
}
}

// ---- Shutdown.

rt_test! {
async fn shutdown_drains_pending_puts() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &small_config()).await.unwrap();
    let h = repo.create_collection("d", Codec::None).await.unwrap();
    let hold = arm(&h);
    let mut rxs = Vec::new();
    for i in 0..10 {
        rxs.push(h.submit_records(vec![record(i, 30)]).await);
    }
    let r2 = repo.clone();
    let sd = tokio::spawn(async move { r2.shutdown().await });
    hold.add_permits(1);
    hold.add_permits(16);
    let mut saved = Vec::new();
    for rx in rxs {
        saved.extend(h.finish(rx).await.unwrap());
    }
    sd.await.unwrap().unwrap();
    assert!(matches!(h.put(vec![record(99, 5)]).await.err().unwrap(), Error::ShuttingDown));
    assert!(matches!(repo.get(&saved[0]).await.err().unwrap(), Error::ShuttingDown));
    drop((repo, h));
    let (repo, report) = Repository::open(td.repo()).await.unwrap();
    assert_eq!(report.loaded[0].recovery.entries, 11);
    for (i, l) in saved.iter().enumerate() {
        assert_eq!(repo.get(l).await.unwrap(), record(i, 30).as_bytes());
    }
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn shutdown_waits_for_admitted_put() {
    let td = TestDir::new();
    let cfg = Config { max_concurrent_prepares: Some(1), ..Config::default() };
    prepare_repo(&td.repo(), &cfg).await;
    let repo = Repository::open(td.repo()).await.unwrap().0;
    let h = repo.create_collection("w", zstd_codec()).await.unwrap();
    let recs: Vec<Record> = (0..30).map(|i| record(i, 100 * 1024)).collect();
    let h2 = h.clone();
    let put = tokio::spawn(async move { h2.put(recs).await });
    repo.shutdown().await.unwrap();
    let outcome = put.await.unwrap();
    drop((repo, h));
    let (repo, report) = Repository::open(td.repo()).await.unwrap();
    let entries = report.loaded[0].recovery.entries;
    match outcome {
        Ok(at) => {
            assert_eq!(entries, 31);
            assert_eq!(repo.get(&at[7]).await.unwrap(), record(7, 100 * 1024).as_bytes());
        }
        Err(Error::ShuttingDown) => assert_eq!(entries, 1),
        Err(e) => panic!("{e:?}"),
    }
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn cancelled_put_after_queue_is_committed() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &small_config()).await.unwrap();
    let h = repo.create_collection("c", Codec::None).await.unwrap();
    let hold = arm(&h);
    let rx = h.submit_records(vec![record(1, 30)]).await;
    drop(rx);
    hold.add_permits(16);
    repo.shutdown().await.unwrap();
    drop((repo, h));
    let (repo, report) = Repository::open(td.repo()).await.unwrap();
    assert_eq!(report.loaded[0].recovery.entries, 2);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn shutdown_twice_ok() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    repo.create_collection("a", Codec::None).await.unwrap();
    repo.shutdown().await.unwrap();
    repo.shutdown().await.unwrap();
}
}

// ---- Cancellation.

rt_test! {
async fn create_cancelled_mid_flight_still_registers() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    // The gate and the create lock are free, so the create future does not suspend before it has
    // spawned the unit: its first suspension point is the unit's JoinHandle. Cancelling at the
    // first `yield_now` therefore drops the future after the spawn, while the create runs.
    tokio::select! {
        biased;
        r = repo.create_collection("x", Codec::None) => { r.unwrap(); }
        () = tokio::task::yield_now() => {}
    }
    let mut registered = false;
    for _ in 0..5000 {
        if repo.collection("x").is_ok() {
            registered = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    assert!(registered, "created collection was not registered");
    let e = repo.create_collection("x", Codec::None).await.err().unwrap();
    assert!(matches!(e, Error::SlugTaken { .. }), "{e:?}");
    let h = repo.collection("x").unwrap();
    let at = h.put(vec![record(1, 10)]).await.unwrap();
    assert_eq!(repo.get(&at[0]).await.unwrap(), record(1, 10).as_bytes());
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn cancelled_open_keeps_lock_until_jobs_finish() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    repo.create_collection("a", Codec::None).await.unwrap();
    repo.shutdown().await.unwrap();
    drop(repo);
    let gate = hold_open(&td.repo());
    let r = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        Repository::open(td.repo()),
    )
    .await;
    assert!(r.is_err(), "open should still be running");
    // The orphaned recovery job is blocked: the directory must stay locked.
    let e = Repository::open(td.repo()).await.err().unwrap();
    assert!(matches!(e, Error::Locked { .. }), "{e:?}");
    gate.release();
    let mut opened = None;
    for _ in 0..5000 {
        match Repository::open(td.repo()).await {
            Ok((r, _)) => {
                opened = Some(r);
                break;
            }
            Err(Error::Locked { .. }) => {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            Err(e) => panic!("{e:?}"),
        }
    }
    opened.expect("lock never released").shutdown().await.unwrap();
}
}

rt_test! {
async fn cancelled_shutdown_then_called_again() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    h.put(vec![record(1, 10)]).await.unwrap();
    tokio::select! {
        biased;
        r = repo.shutdown() => { r.unwrap(); }
        () = tokio::task::yield_now() => {}
    }
    repo.shutdown().await.unwrap();
    assert!(matches!(h.put(vec![record(2, 10)]).await.err().unwrap(), Error::ShuttingDown));
    let (again, report) = Repository::open(td.repo()).await.unwrap();
    assert_eq!(report.loaded[0].recovery.entries, 2);
    again.shutdown().await.unwrap();
}
}

// ---- Commit failures (injected).

use crate::collection::CollectionHandle;
use crate::writer::Fault;

fn script(h: &CollectionHandle, faults: &[Fault]) {
    h.shared
        .hooks
        .script
        .lock()
        .unwrap()
        .extend(faults.iter().copied());
}

/// Queues three requests of 1, 2 and 3 records (ids 0..6) into one group and waits for them.
async fn three_requests(h: &CollectionHandle) -> Vec<Result<Vec<Located>, Error>> {
    let hold = arm(h);
    let mut rxs = Vec::new();
    let mut next = 0usize;
    for n in 1..=3usize {
        let recs: Vec<Record> = (0..n).map(|k| record(next + k, 30)).collect();
        next += n;
        rxs.push(h.submit_records(recs).await);
    }
    hold.add_permits(100);
    let mut out = Vec::new();
    for rx in rxs {
        out.push(h.finish(rx).await);
    }
    *h.shared.hooks.hold.lock().unwrap() = None;
    out
}

fn check_ids(at: &[Located], first: usize) {
    for (k, l) in at.iter().enumerate() {
        assert_eq!(&*l.entry.record_id, id_str(first + k));
        if k > 0 {
            assert_eq!(
                l.entry.offset,
                at[k - 1].entry.offset + at[k - 1].entry.raw_size
            );
        }
    }
}

async fn writer_repo(td: &TestDir) -> (Repository, CollectionHandle) {
    let repo = Repository::init_with_config(td.repo(), &small_config())
        .await
        .unwrap();
    let h = repo.create_collection("w", Codec::None).await.unwrap();
    (repo, h)
}

rt_test! {
async fn poisoned_commit_reopens_and_retries_group() {
    let td = TestDir::new();
    let (repo, h) = writer_repo(&td).await;
    let before = h.shared.published.load();
    let c0 = h.stats().commits;
    script(&h, &[Fault::Poison]);
    let res = three_requests(&h).await;
    let lens: Vec<usize> = res.iter().map(|r| r.as_ref().unwrap().len()).collect();
    assert_eq!(lens, [1, 2, 3]);
    check_ids(res[0].as_ref().unwrap(), 0);
    check_ids(res[1].as_ref().unwrap(), 1);
    check_ids(res[2].as_ref().unwrap(), 3);
    assert_eq!(h.stats().commits - c0, 1);
    assert!(!Arc::ptr_eq(&before, &h.shared.published.load()), "snapshot not republished");
    assert_eq!(live_entries(&h).await.len(), 7);
    let at = &res[2].as_ref().unwrap()[2];
    assert_eq!(repo.get(at).await.unwrap(), record(5, 30).as_bytes());
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn poisoned_retry_failure_fails_whole_group() {
    let td = TestDir::new();
    let (repo, h) = writer_repo(&td).await;
    script(&h, &[Fault::Poison, Fault::Poison]);
    let res = three_requests(&h).await;
    for r in &res {
        assert!(matches!(r, Err(Error::Commit { .. })), "{r:?}");
    }
    assert_eq!(live_entries(&h).await.len(), 1);
    let at = h.put(vec![record(9, 10)]).await.unwrap();
    assert_eq!(repo.get(&at[0]).await.unwrap(), record(9, 10).as_bytes());
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rejected_group_falls_back_to_per_request_commits() {
    let td = TestDir::new();
    let (repo, h) = writer_repo(&td).await;
    let c0 = h.stats().commits;
    script(&h, &[Fault::Reject]);
    let res = three_requests(&h).await;
    check_ids(res[0].as_ref().unwrap(), 0);
    check_ids(res[1].as_ref().unwrap(), 1);
    check_ids(res[2].as_ref().unwrap(), 3);
    assert_eq!(h.stats().commits - c0, 3);
    assert_eq!(live_entries(&h).await.len(), 7);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn one_rejected_request_fails_alone() {
    let td = TestDir::new();
    let (repo, h) = writer_repo(&td).await;
    script(&h, &[Fault::Reject, Fault::Reject]);
    let res = three_requests(&h).await;
    assert!(matches!(res[0], Err(Error::Commit { .. })), "{:?}", res[0]);
    check_ids(res[1].as_ref().unwrap(), 1);
    check_ids(res[2].as_ref().unwrap(), 3);
    let ids: Vec<String> =
        live_entries(&h).await.iter().skip(1).map(|e| e.record_id.to_string()).collect();
    assert_eq!(ids, (1..6).map(id_str).collect::<Vec<_>>());
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn poison_during_isolation_fails_the_rest() {
    let td = TestDir::new();
    let (repo, h) = writer_repo(&td).await;
    script(&h, &[Fault::Reject, Fault::Reject, Fault::Poison]);
    let res = three_requests(&h).await;
    assert!(matches!(res[0], Err(Error::Commit { .. })));
    assert!(matches!(res[1], Err(Error::Commit { .. })));
    assert!(matches!(res[2], Err(Error::Commit { .. })));
    assert_eq!(live_entries(&h).await.len(), 1);
    let at = h.put(vec![record(9, 10)]).await.unwrap();
    assert_eq!(repo.get(&at[0]).await.unwrap(), record(9, 10).as_bytes());
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn failed_shutdown_is_reported_to_every_caller() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    h.shared.hooks.panic_on_exit.store(true, Ordering::Release);
    let (r1, r2) = (repo.clone(), repo.clone());
    let (a, b) = tokio::join!(r1.shutdown(), r2.shutdown());
    for r in [a, b, repo.shutdown().await] {
        let e = r.expect_err("shutdown must fail");
        assert!(
            matches!(&e, Error::ShutdownFailed(inner) if matches!(**inner, Error::Join(_))),
            "{e:?}"
        );
        assert_eq!(e.kind(), ErrorKind::Unavailable);
    }
}
}
