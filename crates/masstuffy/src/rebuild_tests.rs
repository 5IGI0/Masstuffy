//! Tests of dictionary training and of the rebuild orchestration (M20).

use std::collections::BTreeMap;
use std::fs;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use masstuffy_cdx::Entry;
use masstuffy_collection::{
    Codec, CollectionId, RawCheck, RawEncoding, SkipReason as RecordSkipReason, Unreadable,
};
use masstuffy_zframe::{DecoderDict, Decompressor};
use tokio::sync::Semaphore;

use crate::collection::CollectionHandle;
use crate::config::Config;
use crate::error::{Error, ErrorKind};
use crate::rebuild::{RebuildOptions, RebuildReport};
use crate::repository::Repository;
use crate::testutil::*;
use crate::train::TrainOptions;
use crate::types::{Located, RawItem};
use crate::writer::{Fault, RebuildPoint};

macro_rules! rt_test {
    ($(#[$m:meta])* async fn $name:ident() $body:block) => {
        $(#[$m])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            bounded(async $body).await
        }
    };
}

// ---- Helpers.

fn config() -> Config {
    Config {
        rebuild_workers: Some(2),
        ..small_config()
    }
}

fn coll_dir(repo: &Repository, id: CollectionId) -> PathBuf {
    repo.path().join("data/repository").join(id.to_string())
}

fn dict_dir(repo: &Repository) -> PathBuf {
    repo.path().join("data/dict/zstd")
}

fn dict_files(dir: &Path) -> usize {
    fs::read_dir(dir).unwrap().count()
}

/// Names of the files of generation `g` in `dir`.
fn gen_files(dir: &Path, g: u32) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| {
            [
                format!("seg.{g}."),
                format!("index.{g}."),
                format!("scan.{g}."),
            ]
            .iter()
            .any(|p| n.starts_with(p.as_str()))
        })
        .collect();
    v.sort();
    v
}

fn body_len(i: usize) -> usize {
    300 + (i * 131) % 2400
}

fn rec(i: usize) -> masstuffy_warc::Record {
    record(i, body_len(i))
}

/// Puts records `base..base + n` in batches of 25; returns their locators.
async fn fill(h: &CollectionHandle, base: usize, n: usize) -> Vec<Located> {
    let mut out = Vec::with_capacity(n);
    let ids: Vec<usize> = (base..base + n).collect();
    for chunk in ids.chunks(25) {
        out.extend(
            h.put(chunk.iter().map(|&i| rec(i)).collect())
                .await
                .unwrap(),
        );
    }
    out
}

/// A dictionary that differs from `base_dict()`, trained once per process.
fn second_dict() -> &'static [u8] {
    static D: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    D.get_or_init(|| {
        let mut set = masstuffy_zframe::SampleSet::new(masstuffy_zframe::TrainOptions {
            max_dict_size: 4 * 1024,
            ..masstuffy_zframe::TrainOptions::default()
        });
        for i in 0..400usize {
            let s = format!(
                "HTTP/1.1 200 OK\r\nServer: other-{}\r\nX-Id: {i}\r\n\r\n{}",
                i % 7,
                String::from_utf8(letters(i + 5000, 120)).unwrap()
            );
            set.push(s.as_bytes()).unwrap();
        }
        masstuffy_zframe::train(&set).unwrap()
    })
}

/// Installs `second_dict()`; returns its id and the stored bytes.
fn install_second(repo: &Repository) -> (u32, Vec<u8>) {
    let inst = repo
        .core()
        .resolver
        .0
        .install("second", second_dict())
        .unwrap();
    let path = dict_dir(repo).join(format!("second.{}.zstdict", inst.dict_id));
    (inst.dict_id, fs::read(path).unwrap())
}

fn zstd(dict: u32, level: i32) -> Codec {
    Codec::Zstd {
        dict_id: NonZeroU32::new(dict).unwrap(),
        level,
    }
}

fn tiny() -> TrainOptions {
    TrainOptions {
        max_dict_size: 4096,
        ..TrainOptions::default()
    }
}

/// Trains a small dictionary on `h` and returns its id.
async fn train(h: &CollectionHandle) -> u32 {
    h.train_dictionary(tiny()).await.unwrap().dict_id
}

fn ro() -> RebuildOptions {
    RebuildOptions::default()
}

/// Every record `base..base + n` is found by record id (after `sync_index`) with its exact bytes.
async fn read_all_exact(repo: &Repository, h: &CollectionHandle, ids: &[usize]) {
    repo.sync_index().await.unwrap();
    for &i in ids {
        let (_, bytes) = repo
            .get_by_id(&id_str(i), Some(h.id()))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("record {i} not found"));
        assert_eq!(bytes, rec(i).as_bytes(), "record {i}");
    }
}

/// The CDX holds each of `ids` exactly once (warcinfo records are ignored) and nothing else.
async fn assert_live_exactly(h: &CollectionHandle, ids: &[usize]) {
    let mut count: BTreeMap<String, usize> = BTreeMap::new();
    for e in live_entries(h).await {
        if &*e.warc_type != "warcinfo" {
            *count.entry(e.record_id.to_string()).or_default() += 1;
        }
    }
    let want: BTreeMap<String, usize> = ids.iter().map(|&i| (id_str(i), 1)).collect();
    assert_eq!(count, want);
}

fn pause_at(h: &CollectionHandle, p: RebuildPoint) -> Arc<Semaphore> {
    let s = Arc::new(Semaphore::new(0));
    *h.shared.hooks.rebuild_pause.lock().unwrap() = Some((p, Arc::clone(&s)));
    s
}

fn unpause(h: &CollectionHandle) {
    *h.shared.hooks.rebuild_pause.lock().unwrap() = None;
}

fn crash_at(h: &CollectionHandle, p: RebuildPoint) {
    *h.shared.hooks.rebuild_crash.lock().unwrap() = Some(p);
}

async fn reached(h: &CollectionHandle, p: RebuildPoint) {
    let mut rx = h.shared.hooks.rebuild_reached.subscribe();
    rx.wait_for(|v| *v == Some(p)).await.unwrap();
}

fn last_point(h: &CollectionHandle) -> Option<RebuildPoint> {
    *h.shared.hooks.rebuild_reached.borrow()
}

fn arm(repo: &Repository) -> Arc<Semaphore> {
    let s = Arc::new(Semaphore::new(0));
    *index_hooks(repo).hold.lock().unwrap() = Some(Arc::clone(&s));
    s
}

fn disarm(repo: &Repository, held: &Semaphore) {
    *index_hooks(repo).hold.lock().unwrap() = None;
    held.add_permits(1 << 16);
}

fn spawn_rebuild(
    h: &CollectionHandle,
    target: Codec,
    opts: RebuildOptions,
) -> tokio::task::JoinHandle<Result<RebuildReport, Error>> {
    let h = h.clone();
    tokio::spawn(async move { h.rebuild(target, opts).await })
}

async fn reopen(repo: Repository, td: &TestDir) -> Repository {
    repo.shutdown().await.unwrap();
    drop(repo);
    Repository::open(td.repo()).await.unwrap().0
}

fn index_staged(repo: &Repository, h: &CollectionHandle) -> Option<masstuffy_index::Checkpoint> {
    repo.core()
        .index()
        .unwrap()
        .state(masstuffy_index::CollectionKey::new(h.id().as_u128()))
        .unwrap()
        .and_then(|s| s.staged_checkpoint)
}

// ---- Training.

#[test]
fn make_label_rules() {
    use masstuffy_warc::WarcDate;
    let now = WarcDate::parse("2026-11-14T10:20:30Z").unwrap();
    assert_eq!(
        crate::train::make_label("news", &now),
        "news_20261114102030"
    );
    assert_eq!(
        crate::train::make_label("a.b/c\u{e9}", &now),
        "a_b_c___20261114102030"
    );
    let long = "x".repeat(200);
    let l = crate::train::make_label(&long, &now);
    assert_eq!(l.len(), 128);
    assert!(masstuffy_dictstore::is_valid_label(&l));
    assert!(l.ends_with("_20261114102030"));
}

rt_test! {
async fn train_deterministic() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &config()).await.unwrap();
    let h = repo.create_collection("t", Codec::None).await.unwrap();
    fill(&h, 1, 400).await;
    let a = h.train_dictionary(tiny()).await.unwrap();
    let b = h.train_dictionary(tiny()).await.unwrap();
    assert!(a.created);
    assert!(!b.created);
    assert_eq!(a.dict_id, b.dict_id);
    assert_eq!((a.generation, a.cdx_len), (b.generation, b.cdx_len));
    let file = dict_dir(&repo).join(format!("{}.{}.zstdict", a.label, a.dict_id));
    let bytes = fs::read(&file).unwrap();
    assert_eq!(u64::try_from(bytes.len()).unwrap(), a.dict_size);
    let before = dict_files(&dict_dir(&repo));

    let repo = reopen(repo, &td).await;
    let h = repo.collection("t").unwrap();
    let c = h.train_dictionary(tiny()).await.unwrap();
    assert_eq!(c.dict_id, a.dict_id);
    assert!(!c.created);
    assert_eq!(dict_files(&dict_dir(&repo)), before);

    // The state changed: the report says which state it sampled.
    h.put(vec![rec(1000)]).await.unwrap();
    let d = h.train_dictionary(tiny()).await.unwrap();
    assert_ne!(d.cdx_len, c.cdx_len);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn train_bounds_and_report() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &config()).await.unwrap();
    let h = repo.create_collection("bounds", Codec::None).await.unwrap();
    fill(&h, 1, 400).await;
    let r = h
        .train_dictionary(TrainOptions { max_samples: 50, ..tiny() })
        .await
        .unwrap();
    assert_eq!(r.candidates, 400);
    assert_eq!(r.samples, 50);
    assert_eq!(r.skipped, 0);

    let h2 = repo.create_collection("bytes", Codec::None).await.unwrap();
    h2.put((1..=600).map(|i| record(i, 2048)).collect()).await.unwrap();
    let r = h2
        .train_dictionary(TrainOptions {
            max_total_bytes: 1 << 20,
            max_sample_bytes: 1024,
            ..tiny()
        })
        .await
        .unwrap();
    assert!(r.sample_bytes <= 1 << 20);
    assert_eq!(r.samples, 600);
    assert_eq!(r.sample_bytes, 600 * 1024);
    let (slug, stamp) = r.label.rsplit_once('_').unwrap();
    assert_eq!(slug, "bytes");
    assert_eq!(stamp.len(), 14);
    assert!(stamp.bytes().all(|b| b.is_ascii_digit()));
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn train_skips_large_and_too_few() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &config()).await.unwrap();
    let h = repo.create_collection("few", Codec::None).await.unwrap();
    fill(&h, 1, 5).await;
    h.put((100..103).map(|i| record(i, 1_500_000)).collect()).await.unwrap();
    let before = dict_files(&dict_dir(&repo));
    let e = h.train_dictionary(tiny()).await.err().unwrap();
    assert!(
        matches!(e, Error::Train(masstuffy_zframe::TrainError::TooFewSamples)),
        "{e:?}"
    );
    assert_eq!(e.kind(), ErrorKind::Invalid);
    assert_eq!(dict_files(&dict_dir(&repo)), before);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn train_invalid_options() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &config()).await.unwrap();
    let h = repo.create_collection("opt", Codec::None).await.unwrap();
    let d = TrainOptions::default();
    let bad: [(&str, TrainOptions); 9] = [
        ("max_samples", TrainOptions { max_samples: 7, ..d }),
        ("max_samples", TrainOptions { max_samples: 1_000_001, ..d }),
        ("max_sample_bytes", TrainOptions { max_sample_bytes: 0, ..d }),
        ("max_sample_bytes", TrainOptions { max_sample_bytes: (1 << 20) + 1, ..d }),
        ("max_total_bytes", TrainOptions { max_total_bytes: (1 << 20) - 1, ..d }),
        ("max_total_bytes", TrainOptions { max_total_bytes: 513 << 20, ..d }),
        ("max_dict_size", TrainOptions { max_dict_size: 255, ..d }),
        ("max_dict_size", TrainOptions { max_dict_size: (64 << 20) + 1, ..d }),
        ("max_samples", TrainOptions { max_samples: 0, ..d }),
    ];
    for (field, o) in bad {
        let e = h.train_dictionary(o).await.err().unwrap();
        assert!(matches!(e, Error::InvalidOptions(f) if f == field), "{field}: {e:?}");
        assert_eq!(e.kind(), ErrorKind::Invalid);
    }
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn train_concurrent() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &config()).await.unwrap();
    let h = repo.create_collection("t", Codec::None).await.unwrap();
    let other = repo.create_collection("other", Codec::None).await.unwrap();
    fill(&h, 1, 300).await;
    let mut putters = Vec::new();
    for t in 0..4 {
        let other = other.clone();
        putters.push(tokio::spawn(async move {
            for j in 0..20 {
                other.put(vec![record(t * 1000 + j, 200)]).await.unwrap();
            }
        }));
    }
    let (a, b) = (h.clone(), h.clone());
    let ta = tokio::spawn(async move { a.train_dictionary(tiny()).await.unwrap() });
    let tb = tokio::spawn(async move { b.train_dictionary(tiny()).await.unwrap() });
    let (ra, rb) = (ta.await.unwrap(), tb.await.unwrap());
    for p in putters {
        p.await.unwrap();
    }
    assert_eq!(ra.dict_id, rb.dict_id);
    assert!(ra.created ^ rb.created);
    repo.shutdown().await.unwrap();
}
}

// ---- Rebuild: end to end.

rt_test! {
async fn rebuild_none_to_zstd_end_to_end() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &config()).await.unwrap();
    let h = repo.create_collection("r", Codec::None).await.unwrap();
    let old = fill(&h, 1, 500).await;
    let dict = train(&h).await;
    let rep = h.rebuild(zstd(dict, 3), ro()).await.unwrap();
    assert_eq!((rep.copy.from_generation, rep.copy.generation), (1, 2));
    assert_eq!(rep.copy.copied, 500 + 1);
    assert!(rep.index_switched);
    assert!(rep.old_files_removed.is_some_and(|n| n >= 2));
    let info = h.info();
    assert_eq!((info.generation, info.codec), (2, zstd(dict, 3)));
    assert!(gen_files(&coll_dir(&repo, h.id()), 1).is_empty());
    let ids: Vec<usize> = (1..=500).collect();
    read_all_exact(&repo, &h, &ids).await;
    assert_live_exactly(&h, &ids).await;
    let e = repo.get(&old[0]).await.err().unwrap();
    assert!(matches!(e, Error::StaleLocator { expected: 2, got: 1 }), "{e:?}");
    // Puts go to generation 2.
    let l = h.put(vec![rec(900)]).await.unwrap();
    assert_eq!(l[0].generation, 2);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rebuild_with_concurrent_puts() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &config()).await.unwrap();
    let h = repo.create_collection("c", Codec::None).await.unwrap();
    fill(&h, 1, 200).await;
    let dict = train(&h).await;
    let acked: Arc<std::sync::Mutex<Vec<(usize, Located)>>> = Arc::default();
    let put_batch = |base: usize| {
        let (h, acked) = (h.clone(), Arc::clone(&acked));
        async move {
            let mut joins = Vec::new();
            for t in 0..4 {
                let (h, acked) = (h.clone(), Arc::clone(&acked));
                joins.push(tokio::spawn(async move {
                    for j in 0..50 {
                        let i = base + t * 100 + j;
                        let l = h.put(vec![rec(i)]).await.unwrap();
                        acked.lock().unwrap().push((i, l[0].clone()));
                    }
                }));
            }
            for j in joins {
                j.await.unwrap();
            }
        }
    };

    let p1 = pause_at(&h, RebuildPoint::Started);
    let rb = spawn_rebuild(&h, zstd(dict, 1), ro());
    reached(&h, RebuildPoint::Started).await;
    put_batch(10_000).await;
    let p2 = pause_at(&h, RebuildPoint::Copied);
    p1.add_permits(1);
    reached(&h, RebuildPoint::Copied).await;
    put_batch(20_000).await;
    // Writers keep putting until the rebuild returns.
    let stop = Arc::new(AtomicBool::new(false));
    let mut loopers = Vec::new();
    for t in 0..4usize {
        let (h, acked, stop) = (h.clone(), Arc::clone(&acked), Arc::clone(&stop));
        loopers.push(tokio::spawn(async move {
            let mut j = 0;
            while !stop.load(Ordering::Acquire) {
                let i = 30_000 + t * 10_000 + j;
                let l = h.put(vec![rec(i)]).await.unwrap();
                acked.lock().unwrap().push((i, l[0].clone()));
                j += 1;
            }
        }));
    }
    p2.add_permits(1);
    let rep = rb.await.unwrap().unwrap();
    stop.store(true, Ordering::Release);
    for l in loopers {
        l.await.unwrap();
    }
    assert_eq!(rep.copy.generation, 2);

    let acked = acked.lock().unwrap().clone();
    let mut ids: Vec<usize> = (1..=200).collect();
    ids.extend(acked.iter().map(|(i, _)| *i));
    assert_live_exactly(&h, &ids).await;
    read_all_exact(&repo, &h, &ids).await;
    for (i, l) in acked.iter().filter(|(_, l)| l.generation == 2).take(100) {
        assert_eq!(repo.get(l).await.unwrap(), rec(*i).as_bytes());
    }
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rebuild_reads_never_fail() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &config()).await.unwrap();
    let h = repo.create_collection("rd", Codec::None).await.unwrap();
    let old = fill(&h, 1, 300).await;
    let dict = train(&h).await;
    repo.sync_index().await.unwrap();

    let p_started = pause_at(&h, RebuildPoint::Started);
    let rb = spawn_rebuild(&h, zstd(dict, 1), ro());
    reached(&h, RebuildPoint::Started).await;
    // The index (staged) is in place; from here its passes are held.
    let held = arm(&repo);
    let p_fin = pause_at(&h, RebuildPoint::Finished);
    p_started.add_permits(1);

    let stop = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();
    for t in 0..4usize {
        let (repo, h, stop) = (repo.clone(), h.clone(), Arc::clone(&stop));
        let old = old.clone();
        readers.push(tokio::spawn(async move {
            let mut n = 0usize;
            while !stop.load(Ordering::Acquire) {
                let i = (n * 7 + t) % 300;
                let r1 = repo.get(&old[i]).await;
                let r2 = repo.get_by_id(&id_str(i + 1), Some(h.id())).await;
                for e in [r1.as_ref().err(), r2.as_ref().err()].into_iter().flatten() {
                    assert!(matches!(e, Error::StaleLocator { .. }), "{e:?}");
                    // Only after the switch was reached.
                    assert_eq!(last_point(&h), Some(RebuildPoint::Switched), "{e:?}");
                }
                if let Ok(bytes) = &r1 {
                    assert_eq!(bytes, rec(i + 1).as_bytes());
                }
                n += 1;
                tokio::task::yield_now().await;
            }
        }));
    }
    reached(&h, RebuildPoint::Finished).await;
    // Paused after the manifest swap, the index not switched: generation 1 is still served.
    assert_eq!(h.info().generation, 2);
    for (i, l) in old.iter().enumerate().take(50) {
        assert_eq!(l.generation, 1);
        assert_eq!(repo.get(l).await.unwrap(), rec(i + 1).as_bytes());
        let (hit, bytes) = repo
            .get_by_id(&id_str(i + 1), Some(h.id()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hit.generation, 1);
        assert_eq!(bytes, rec(i + 1).as_bytes());
    }
    disarm(&repo, &held);
    p_fin.add_permits(1);
    let rep = rb.await.unwrap().unwrap();
    assert!(rep.index_switched);
    stop.store(true, Ordering::Release);
    for r in readers {
        r.await.unwrap();
    }
    repo.sync_index().await.unwrap();
    let (hit, bytes) = repo.get_by_id(&id_str(1), Some(h.id())).await.unwrap().unwrap();
    assert_eq!(hit.generation, 2);
    assert_eq!(bytes, rec(1).as_bytes());
    let e = repo.get(&old[0]).await.err().unwrap();
    assert!(matches!(e, Error::StaleLocator { .. }), "{e:?}");
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rebuild_stale_put_reprepared() {
    let td = TestDir::new();
    prepare_repo(&td.repo(), &config()).await;
    let repo = Repository::open_inner(td.repo(), RawCheck::Full).await.unwrap().0;
    let h = repo.create_collection("s", Codec::None).await.unwrap();
    fill(&h, 1, 50).await;
    let dict = train(&h).await;

    // A put prepared for generation 1 and held back.
    let sem = Arc::new(Semaphore::new(0));
    *h.shared.hooks.after_prepare.lock().unwrap() = Some(Arc::clone(&sem));
    let mut rx = h.shared.hooks.after_prepare_reached.subscribe();
    let base = *rx.borrow();
    let hp = h.clone();
    let put = tokio::spawn(async move { hp.put(vec![rec(500)]).await });
    rx.wait_for(|n| *n > base).await.unwrap();
    *h.shared.hooks.after_prepare.lock().unwrap() = None;
    let rep = h.rebuild(zstd(dict, 1), ro()).await.unwrap();
    assert_eq!(rep.copy.generation, 2);
    sem.add_permits(1);
    let l = put.await.unwrap().unwrap();
    assert_eq!(l[0].generation, 2);
    assert_eq!(repo.get(&l[0]).await.unwrap(), rec(500).as_bytes());
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rebuild_stale_put_raw_dictionary_frame() {
    let td = TestDir::new();
    prepare_repo(&td.repo(), &config()).await;
    let repo = Repository::open_inner(td.repo(), RawCheck::Full).await.unwrap().0;
    let h = repo.create_collection("raw", zstd_codec()).await.unwrap();
    fill(&h, 1, 20).await;
    let (d2, _) = install_second(&repo);
    assert_ne!(d2, dict_id());

    let r = rec(700);
    let payload = frame(base_dict(), r.as_bytes());
    let item = RawItem {
        meta: meta_for(&r, &payload, true),
        payload,
    };
    let sem = Arc::new(Semaphore::new(0));
    *h.shared.hooks.after_prepare.lock().unwrap() = Some(Arc::clone(&sem));
    let mut rx = h.shared.hooks.after_prepare_reached.subscribe();
    let base = *rx.borrow();
    let hp = h.clone();
    let put = tokio::spawn(async move { hp.put_raw(vec![item]).await });
    rx.wait_for(|n| *n > base).await.unwrap();
    *h.shared.hooks.after_prepare.lock().unwrap() = None;
    h.rebuild(zstd(d2, 1), ro()).await.unwrap();
    sem.add_permits(1);
    let l = put.await.unwrap().unwrap();
    assert_eq!(l[0].generation, 2);
    let raw = repo.get_raw(&l[0]).await.unwrap();
    assert_eq!(raw.encoding, RawEncoding::Zstd { dict_id: d2 });
    assert_eq!(repo.get(&l[0]).await.unwrap(), r.as_bytes());
    repo.shutdown().await.unwrap();
}
}

// ---- Crash points.

rt_test! {
async fn rebuild_crash_before_finish() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &config()).await.unwrap();
    let h = repo.create_collection("cr", Codec::None).await.unwrap();
    fill(&h, 1, 100).await;
    let dict = train(&h).await;
    crash_at(&h, RebuildPoint::Copied);
    let e = h.rebuild(zstd(dict, 1), ro()).await.err().unwrap();
    assert!(matches!(e, Error::Internal("injected rebuild crash")), "{e:?}");
    let dir = coll_dir(&repo, h.id());
    assert!(!gen_files(&dir, 2).is_empty());

    let repo = reopen(repo, &td).await;
    let h = repo.collection("cr").unwrap();
    assert_eq!(h.info().generation, 1);
    assert!(gen_files(&coll_dir(&repo, h.id()), 2).is_empty());
    let ids: Vec<usize> = (1..=100).collect();
    read_all_exact(&repo, &h, &ids).await;
    assert_eq!(index_dump(&repo, h.id()).await, live_entries(&h).await);
    assert_eq!(index_staged(&repo, &h), None);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rebuild_crash_after_swap_before_index_switch() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &config()).await.unwrap();
    let h = repo.create_collection("cs", Codec::None).await.unwrap();
    fill(&h, 1, 100).await;
    let dict = train(&h).await;
    repo.sync_index().await.unwrap();
    let p = pause_at(&h, RebuildPoint::Started);
    let rb = spawn_rebuild(&h, zstd(dict, 1), ro());
    reached(&h, RebuildPoint::Started).await;
    let held = arm(&repo);
    crash_at(&h, RebuildPoint::Finished);
    p.add_permits(1);
    let e = rb.await.unwrap().err().unwrap();
    assert!(matches!(e, Error::Internal("injected rebuild crash")), "{e:?}");
    // The indexer dies without applying anything more.
    index_hooks(&repo).abandon.store(true, Ordering::Release);
    disarm(&repo, &held);
    let _ = repo.shutdown().await;
    drop(h);
    let (repo, _) = Repository::open(td.repo()).await.unwrap();
    let h = repo.collection("cs").unwrap();
    assert_eq!(h.info().generation, 2);
    let dir = coll_dir(&repo, h.id());
    assert!(gen_files(&dir, 1).is_empty());
    let ids: Vec<usize> = (1..=100).collect();
    read_all_exact(&repo, &h, &ids).await;
    assert_eq!(index_dump(&repo, h.id()).await, live_entries(&h).await);
    #[cfg(feature = "redb")]
    assert_eq!(index_hooks(&repo).reindexes.load(Ordering::Acquire), 0);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rebuild_crash_after_switch_before_remove() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &config()).await.unwrap();
    let h = repo.create_collection("cw", Codec::None).await.unwrap();
    fill(&h, 1, 100).await;
    let dict = train(&h).await;
    crash_at(&h, RebuildPoint::Switched);
    let e = h.rebuild(zstd(dict, 1), ro()).await.err().unwrap();
    assert!(matches!(e, Error::Internal("injected rebuild crash")), "{e:?}");
    let dir = coll_dir(&repo, h.id());
    assert!(!gen_files(&dir, 1).is_empty());

    let repo = reopen(repo, &td).await;
    let h = repo.collection("cw").unwrap();
    assert_eq!(h.info().generation, 2);
    assert!(gen_files(&coll_dir(&repo, h.id()), 1).is_empty());
    let ids: Vec<usize> = (1..=100).collect();
    read_all_exact(&repo, &h, &ids).await;
    assert_eq!(index_dump(&repo, h.id()).await, live_entries(&h).await);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rebuild_writer_poison_aborts() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &config()).await.unwrap();
    let h = repo.create_collection("wp", Codec::None).await.unwrap();
    fill(&h, 1, 50).await;
    let dict = train(&h).await;
    repo.sync_index().await.unwrap();
    let p = pause_at(&h, RebuildPoint::Started);
    let rb = spawn_rebuild(&h, zstd(dict, 1), ro());
    reached(&h, RebuildPoint::Started).await;
    // The writer reopens the collection, which takes the `Rebuilder`.
    h.shared.hooks.script.lock().unwrap().push_back(Fault::Poison);
    h.put(vec![rec(900)]).await.unwrap();
    unpause(&h);
    p.add_permits(1);
    let e = rb.await.unwrap().err().unwrap();
    assert!(matches!(e, Error::RebuildAborted { .. }), "{e:?}");
    assert_eq!(e.kind(), ErrorKind::Conflict);
    assert_eq!(h.info().generation, 1);
    h.put(vec![rec(901)]).await.unwrap();
    assert!(gen_files(&coll_dir(&repo, h.id()), 2).is_empty());
    repo.sync_index().await.unwrap();
    assert_eq!(index_staged(&repo, &h), None);
    let mut ids: Vec<usize> = (1..=50).collect();
    ids.extend([900, 901]);
    read_all_exact(&repo, &h, &ids).await;
    // The collection can be rebuilt afterwards.
    let rep = h.rebuild(zstd(dict, 1), ro()).await.unwrap();
    assert_eq!(rep.copy.generation, 2);
    repo.shutdown().await.unwrap();
}
}

// ---- Codec matrix.

rt_test! {
async fn rebuild_zstd_d1_to_d2() {
    let td = TestDir::new();
    prepare_repo(&td.repo(), &config()).await;
    let repo = Repository::open_inner(td.repo(), RawCheck::Full).await.unwrap().0;
    let h = repo.create_collection("z", zstd_codec()).await.unwrap();
    let old = fill(&h, 1, 300).await;
    let d2 = train(&h).await;
    assert_ne!(d2, dict_id());
    let rep = h.rebuild(zstd(d2, 1), ro()).await.unwrap();
    assert_eq!(rep.copy.raw_copied, 0);
    let ids: Vec<usize> = (1..=300).collect();
    read_all_exact(&repo, &h, &ids).await;
    for e in live_entries(&h).await {
        let l = Located { collection: h.id(), generation: 2, entry: e.clone() };
        let raw = repo.get_raw(&l).await.unwrap();
        assert_eq!(raw.encoding, RawEncoding::Zstd { dict_id: d2 }, "{}", e.record_id);
    }
    assert!(matches!(repo.get(&old[0]).await, Err(Error::StaleLocator { .. })));
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rebuild_same_dict_compaction() {
    let td = TestDir::new();
    prepare_repo(&td.repo(), &config()).await;
    let repo = Repository::open_inner(td.repo(), RawCheck::Full).await.unwrap().0;
    let h = repo.create_collection("k", zstd_codec()).await.unwrap();
    fill(&h, 1, 100).await;
    let live = live_entries(&h).await;
    let dead: Vec<Entry<'static>> = live
        .iter()
        .filter(|e| &*e.warc_type != "warcinfo")
        .take(10)
        .cloned()
        .collect();
    let o = offline_ctx(&repo, &h);
    repo.shutdown().await.unwrap();
    offline_tombstone(&o, &dead);
    let (repo, _) = Repository::open(td.repo()).await.unwrap();
    let h = repo.collection("k").unwrap();
    let rep = h.rebuild(zstd(dict_id(), 5), ro()).await.unwrap();
    assert_eq!(rep.copy.raw_copied, rep.copy.copied);
    assert_eq!(rep.copy.dropped_tombstoned, 10);
    assert_eq!(rep.copy.tombstone_lines, 10);
    assert_eq!(h.info().codec, zstd(dict_id(), 5));
    let dead_ids: Vec<String> = dead.iter().map(|e| e.record_id.to_string()).collect();
    let alive: Vec<usize> = (1..=100)
        .filter(|&i| !dead_ids.contains(&id_str(i)))
        .collect();
    assert_eq!(alive.len(), 90);
    assert_live_exactly(&h, &alive).await;

    let rep = h
        .rebuild(zstd(dict_id(), 5), RebuildOptions { recompress: true, ..ro() })
        .await
        .unwrap();
    assert_eq!(rep.copy.raw_copied, 0);
    read_all_exact(&repo, &h, &alive).await;
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rebuild_header_only_non_warc() {
    let td = TestDir::new();
    prepare_repo(&td.repo(), &config()).await;
    let repo = Repository::open_inner(td.repo(), RawCheck::HeaderOnly).await.unwrap().0;
    let h = repo.create_collection("ho", zstd_codec()).await.unwrap();
    let payloads: Vec<Vec<u8>> = (0..5)
        .map(|i| format!("this is not a WARC record #{i}, only bytes").into_bytes())
        .collect();
    let items: Vec<RawItem> = payloads
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let stored = frame(base_dict(), p);
            RawItem {
                meta: meta_for(&record(800 + i, 50), &stored, true),
                payload: stored,
            }
        })
        .collect();
    h.put_raw(items).await.unwrap();
    fill(&h, 1, 20).await;
    let (d2, d2_bytes) = install_second(&repo);

    // Zstd(D1) -> Zstd(D2): the payloads move as they are.
    let rep = h.rebuild(zstd(d2, 1), ro()).await.unwrap();
    assert_eq!(rep.copy.skipped, 0);
    repo.sync_index().await.unwrap();
    let dec = DecoderDict::new(&d2_bytes).unwrap();
    let mut z = Decompressor::new().unwrap();
    for (i, p) in payloads.iter().enumerate() {
        let l = repo.find_by_id(&id_str(800 + i), Some(h.id())).await.unwrap().unwrap();
        assert_eq!(l.generation, 2);
        let raw = repo.get_raw(&l).await.unwrap();
        let mut out = Vec::new();
        z.decompress(&raw.bytes, Some(&dec), u64::MAX, &mut out).unwrap();
        assert_eq!(&out, p);
    }

    // To Codec::None the payloads are not WARC records: `Fail` aborts, nothing is left.
    let dir = coll_dir(&repo, h.id());
    let e = h.rebuild(Codec::None, ro()).await.err().unwrap();
    assert!(matches!(e, Error::Rebuild { committed: false, .. }), "{e:?}");
    assert_eq!(h.info().generation, 2);
    assert!(gen_files(&dir, 3).is_empty());
    h.put(vec![rec(60)]).await.unwrap();

    // `Skip` drops them and reports.
    let rep = h
        .rebuild(Codec::None, RebuildOptions { on_unreadable: Unreadable::Skip, ..ro() })
        .await
        .unwrap();
    assert_eq!(rep.copy.skipped, 5);
    assert!(rep.copy.skipped_first.iter().all(|s| s.reason == RecordSkipReason::NotWarc));
    let mut ids: Vec<usize> = (1..=20).collect();
    ids.push(60);
    assert_live_exactly(&h, &ids).await;
    repo.shutdown().await.unwrap();
}
}

// ---- Exclusivity, shutdown, cancellation.

rt_test! {
async fn rebuild_in_progress_conflict() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &config()).await.unwrap();
    let h = repo.create_collection("ip", Codec::None).await.unwrap();
    fill(&h, 1, 30).await;
    let p = pause_at(&h, RebuildPoint::Started);
    let rb = spawn_rebuild(&h, Codec::None, ro());
    reached(&h, RebuildPoint::Started).await;
    let e = h.rebuild(Codec::None, ro()).await.err().unwrap();
    assert!(matches!(e, Error::RebuildInProgress(_)), "{e:?}");
    assert_eq!(e.kind(), ErrorKind::Conflict);
    unpause(&h);
    p.add_permits(1);
    let rep = rb.await.unwrap().unwrap();
    assert_eq!(rep.copy.generation, 2);
    // None -> None is a raw copy.
    assert_eq!(rep.copy.raw_copied, rep.copy.copied);
    // The slot is free again.
    h.rebuild(Codec::None, ro()).await.unwrap();
    repo.shutdown().await.unwrap();
}
}

async fn shutdown_cancels(point: RebuildPoint) {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &config())
        .await
        .unwrap();
    let h = repo.create_collection("sd", Codec::None).await.unwrap();
    fill(&h, 1, 60).await;
    let _p = pause_at(&h, point);
    let rb = spawn_rebuild(&h, Codec::None, ro());
    reached(&h, point).await;
    repo.shutdown().await.unwrap();
    let e = rb.await.unwrap().err().unwrap();
    assert!(matches!(e, Error::ShuttingDown), "{e:?}");
    drop((repo, h));
    let (repo, _) = Repository::open(td.repo()).await.unwrap();
    let h = repo.collection("sd").unwrap();
    assert_eq!(h.info().generation, 1);
    assert!(gen_files(&coll_dir(&repo, h.id()), 2).is_empty());
    let ids: Vec<usize> = (1..=60).collect();
    read_all_exact(&repo, &h, &ids).await;
    repo.shutdown().await.unwrap();
}

rt_test! {
async fn rebuild_shutdown_cancels() {
    shutdown_cancels(RebuildPoint::Copied).await;
    shutdown_cancels(RebuildPoint::Started).await;
}
}

rt_test! {
async fn rebuild_index_equals_rebuild_index() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &config()).await.unwrap();
    let h = repo.create_collection("i8", Codec::None).await.unwrap();
    fill(&h, 1, 120).await;
    let dict = train(&h).await;
    h.rebuild(zstd(dict, 1), ro()).await.unwrap();
    fill(&h, 1000, 30).await;
    repo.sync_index().await.unwrap();
    let a = index_dump(&repo, h.id()).await;
    repo.rebuild_index().await.unwrap();
    let b = index_dump(&repo, h.id()).await;
    assert_eq!(a, b);
    assert_eq!(b, live_entries(&h).await);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rebuild_errors() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &config()).await.unwrap();
    let h = repo.create_collection("er", Codec::None).await.unwrap();
    let e = h.rebuild(zstd(12345, 1), ro()).await.err().unwrap();
    assert!(matches!(e, Error::DictNotFound(12345)), "{e:?}");
    let e = h
        .rebuild(Codec::None, RebuildOptions { workers: NonZeroUsize::new(300), ..ro() })
        .await
        .err()
        .unwrap();
    assert!(matches!(e, Error::InvalidOptions("workers")), "{e:?}");
    let e = h
        .rebuild(Codec::None, RebuildOptions { max_inflight_bytes: Some(0), ..ro() })
        .await
        .err()
        .unwrap();
    assert!(matches!(e, Error::InvalidOptions("max_inflight_bytes")), "{e:?}");
    // None of them left the slot busy.
    h.rebuild(Codec::None, ro()).await.unwrap();
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rebuild_cancelled_caller_continues() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &config()).await.unwrap();
    let h = repo.create_collection("cc", Codec::None).await.unwrap();
    fill(&h, 1, 80).await;
    let dict = train(&h).await;
    let p = pause_at(&h, RebuildPoint::Started);
    let rb = spawn_rebuild(&h, zstd(dict, 1), ro());
    reached(&h, RebuildPoint::Started).await;
    rb.abort();
    assert!(rb.await.unwrap_err().is_cancelled());
    p.add_permits(1);
    reached(&h, RebuildPoint::Switched).await;
    // Shutdown waits for the unit (it holds the admin mutex to the end).
    repo.shutdown().await.unwrap();
    drop((repo, h));
    let (repo, _) = Repository::open(td.repo()).await.unwrap();
    let h = repo.collection("cc").unwrap();
    assert_eq!(h.info().generation, 2);
    assert!(gen_files(&coll_dir(&repo, h.id()), 1).is_empty());
    let ids: Vec<usize> = (1..=80).collect();
    read_all_exact(&repo, &h, &ids).await;
    repo.shutdown().await.unwrap();
}
}

#[test]
fn rebuild_error_kinds() {
    use masstuffy_collection::Error as C;
    let id = CollectionId::from_u128(1);
    let mk = |source: C| Error::Rebuild {
        collection: id,
        committed: false,
        source: Box::new(source),
    };
    assert_eq!(mk(C::RebuildCancelled).kind(), ErrorKind::Conflict);
    assert_eq!(mk(C::StaleSource("manifest")).kind(), ErrorKind::Conflict);
    assert_eq!(
        mk(C::LocatorOrder { cdx_offset: 0 }).kind(),
        ErrorKind::Corrupt
    );
    assert_eq!(mk(C::WorkerPanicked).kind(), ErrorKind::Io);
    assert_eq!(Error::StaleGeneration(id).kind(), ErrorKind::Conflict);
    assert_eq!(Error::InvalidOptions("x").kind(), ErrorKind::Invalid);
}

rt_test! {
async fn rebuild_writer_reopen_during_copy() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &config()).await.unwrap();
    let h = repo.create_collection("rc", Codec::None).await.unwrap();
    fill(&h, 1, 200).await;
    let dict = train(&h).await;
    repo.sync_index().await.unwrap();

    // The first copy round stops with the slot mutex held.
    let gate = Arc::new(crate::writer::CopyGate::default());
    *h.shared.hooks.copy_gate.lock().unwrap() = Some(Arc::clone(&gate));
    let rb = spawn_rebuild(&h, zstd(dict, 1), ro());
    let g = Arc::clone(&gate);
    tokio::task::spawn_blocking(move || g.wait_entered()).await.unwrap();

    // A poisoned commit makes the writer reopen: it sets the cancel flag, then blocks on the slot.
    h.shared.hooks.script.lock().unwrap().push_back(Fault::Poison);
    let hp = h.clone();
    let put = tokio::spawn(async move { hp.put(vec![rec(900)]).await });
    while !h.shared.rebuild.cancel.load(Ordering::SeqCst) {
        tokio::task::yield_now().await;
    }
    gate.release();

    let e = rb.await.unwrap().err().unwrap();
    assert!(matches!(e, Error::RebuildAborted { .. }), "{e:?}");
    let l = put.await.unwrap().unwrap();
    assert_eq!(l[0].generation, 1);
    assert_eq!(h.info().generation, 1);
    assert!(gen_files(&coll_dir(&repo, h.id()), 2).is_empty());
    h.put(vec![rec(901)]).await.unwrap();
    repo.sync_index().await.unwrap();
    assert_eq!(index_staged(&repo, &h), None);
    let mut ids: Vec<usize> = (1..=200).collect();
    ids.extend([900, 901]);
    assert_live_exactly(&h, &ids).await;
    read_all_exact(&repo, &h, &ids).await;
    // And the next rebuild works.
    let rep = h.rebuild(zstd(dict, 1), ro()).await.unwrap();
    assert_eq!(rep.copy.generation, 2);
    repo.shutdown().await.unwrap();
}
}
