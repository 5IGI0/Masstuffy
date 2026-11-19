//! Tests of the collection size counters (M27).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use masstuffy_collection::{Codec, CollectionId};

use crate::collection::CollectionHandle;
use crate::config::Config;
use crate::rebuild::RebuildOptions;
use crate::repository::Repository;
use crate::testutil::*;
use crate::types::{CollectionSize, Located};
use crate::writer::RebuildPoint;

macro_rules! rt_test {
    ($(#[$m:meta])* async fn $name:ident() $body:block) => {
        $(#[$m])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            bounded(async $body).await
        }
    };
}

fn config() -> Config {
    Config {
        rebuild_workers: Some(2),
        ..small_config()
    }
}

fn coll_dir(repo: &Repository, id: CollectionId) -> PathBuf {
    repo.path().join("data/repository").join(id.to_string())
}

/// Sum of the lengths of the `seg.<gen>.*` files.
fn disk_seg_bytes(dir: &Path, gen_id: u32) -> u64 {
    let prefix = format!("seg.{gen_id}.");
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .map(|e| e.metadata().unwrap().len())
        .sum()
}

fn raw_sum(l: &[Located]) -> u64 {
    l.iter().map(|x| x.entry.raw_size).sum()
}

/// The truth, read from the CDX: (live entries minus warcinfo, their stored bytes).
async fn truth(h: &CollectionHandle) -> (u64, u64) {
    let live = live_entries(h).await;
    let n = u64::try_from(live.len()).unwrap() - 1;
    (n, live.iter().map(|e| e.raw_size).sum())
}

async fn reopen(repo: Repository, td: &TestDir) -> Repository {
    repo.shutdown().await.unwrap();
    drop(repo);
    Repository::open(td.repo()).await.unwrap().0
}

rt_test! {
async fn size_new_collection() {
    let td = TestDir::new();
    let repo = repo_with_dict(&td, &config()).await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    let s = h.size();
    assert_eq!((s.records, s.records_total, s.tombstones), (0, 0, 0));
    assert!(s.stored_bytes > 0);
    let dir = coll_dir(&repo, h.id());
    assert_eq!(s.stored_bytes, s.segment_bytes);
    assert_eq!(s.segment_bytes, disk_seg_bytes(&dir, 1));
    assert_eq!(s.cdx_bytes, h.stats().cdx_len);
    assert_eq!(s.cdx_bytes, fs::metadata(dir.join("index.1.cdx")).unwrap().len());
    assert_eq!(h.info().size, s);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn size_concurrent_puts_exact() {
    let td = TestDir::new();
    let repo = repo_with_dict(&td, &config()).await;
    for (slug, codec) in [("n", Codec::None), ("z", zstd_codec())] {
        let h = repo.create_collection(slug, codec).await.unwrap();
        let warcinfo_raw = live_entries(&h).await[0].raw_size;
        let mut joins = Vec::new();
        for t in 0..8usize {
            let h = h.clone();
            joins.push(tokio::spawn(async move {
                let mut mine = Vec::new();
                for j in 0..20usize {
                    let base = 1 + t * 1000 + j * 5;
                    let recs = (base..base + 5).map(|i| record(i, 100 + i % 50)).collect();
                    mine.extend(h.put(recs).await.unwrap());
                    // Read-your-writes: no busy loop.
                    let want = u64::try_from(mine.len()).unwrap();
                    assert!(h.size().records_total >= want);
                }
                mine
            }));
        }
        let mut all = Vec::new();
        for j in joins {
            all.extend(j.await.unwrap());
        }
        let s = h.size();
        assert_eq!((s.records, s.records_total), (800, 800), "{slug}");
        assert_eq!(s.stored_bytes, warcinfo_raw + raw_sum(&all), "{slug}");
        let dir = coll_dir(&repo, h.id());
        assert_eq!(s.segment_bytes, disk_seg_bytes(&dir, 1));
        assert_eq!(s.cdx_bytes, h.stats().cdx_len);
        let listed = repo.collections().into_iter().find(|c| c.id == h.id()).unwrap();
        assert_eq!(listed.size, h.size());
    }
    repo.shutdown().await.unwrap();
}
}

/// Puts 10 records, deletes 3, then one of them again. Returns the locators.
async fn delete_scenario(h: &CollectionHandle) -> Vec<Located> {
    let put = h
        .put((1..=10).map(|i| record(i, 120)).collect())
        .await
        .unwrap();
    let before = h.size();
    assert_eq!(h.delete(put[..3].to_vec()).await.unwrap(), 3);
    let s = h.size();
    assert_eq!((s.records, s.records_total, s.tombstones), (7, 10, 3));
    assert_eq!(s.tombstoned_bytes, raw_sum(&put[..3]));
    assert_eq!(s.stored_bytes, before.stored_bytes);
    h.delete(vec![put[0].clone()]).await.unwrap();
    put
}

rt_test! {
async fn size_delete_and_redelete() {
    let td = TestDir::new();
    let repo = repo_with_dict(&td, &config()).await;
    let h = repo.create_collection("d", Codec::None).await.unwrap();
    delete_scenario(&h).await;
    let s = h.size();
    assert_eq!((s.tombstones, s.records), (4, 6));
    // The documented undercount: the truth is 7.
    assert_eq!(truth(&h).await.0, 7);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn size_survives_reopen() {
    let td = TestDir::new();
    let repo = repo_with_dict(&td, &config()).await;
    let h = repo.create_collection("d", Codec::None).await.unwrap();
    delete_scenario(&h).await;
    let size = h.size();
    let id = h.id();
    let repo = reopen(repo, &td).await;
    let h = repo.collection("d").unwrap();
    assert_eq!(h.size(), size);
    repo.shutdown().await.unwrap();
    drop((repo, h));
    fs::remove_file(coll_dir_of(&td, id).join("scan.1.json")).unwrap();
    let (repo, _) = Repository::open(td.repo()).await.unwrap();
    assert_eq!(repo.collection("d").unwrap().size(), size);
    repo.shutdown().await.unwrap();
}
}

fn coll_dir_of(td: &TestDir, id: CollectionId) -> PathBuf {
    td.repo().join("data/repository").join(id.to_string())
}

rt_test! {
async fn size_after_rebuild() {
    let td = TestDir::new();
    let repo = repo_with_dict(&td, &config()).await;
    let h = repo.create_collection("r", Codec::None).await.unwrap();
    let put = h.put((1..=20).map(|i| record(i, 150)).collect()).await.unwrap();
    h.delete(put[..5].to_vec()).await.unwrap();
    h.delete(vec![put[0].clone()]).await.unwrap();
    let report = h.rebuild(Codec::None, RebuildOptions::default()).await.unwrap();
    assert_eq!(h.info().generation, 2);
    let s = h.size();
    assert_eq!((s.tombstones, s.tombstoned_bytes), (0, 0));
    assert_eq!(s.records, s.records_total);
    assert_eq!(s.records, report.copy.copied);
    assert_eq!(s.records, 16);
    let (n, bytes) = truth(&h).await;
    assert_eq!(s.records, n);
    assert_eq!(s.stored_bytes, bytes);
    let dir = coll_dir(&repo, h.id());
    assert_eq!(s.segment_bytes, disk_seg_bytes(&dir, 2));
    assert_eq!(s.cdx_bytes, h.stats().cdx_len);
    let repo = reopen(repo, &td).await;
    assert_eq!(repo.collection("r").unwrap().size(), s);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn size_puts_during_rebuild() {
    let td = TestDir::new();
    let repo = repo_with_dict(&td, &config()).await;
    let h = repo.create_collection("p", Codec::None).await.unwrap();
    let put = h.put((1..=10).map(|i| record(i, 150)).collect()).await.unwrap();
    h.delete(put[..2].to_vec()).await.unwrap();
    let sem = {
        let s = Arc::new(tokio::sync::Semaphore::new(0));
        *h.shared.hooks.rebuild_pause.lock().unwrap() = Some((RebuildPoint::Copied, Arc::clone(&s)));
        s
    };
    let rb = {
        let h = h.clone();
        tokio::spawn(async move { h.rebuild(Codec::None, RebuildOptions::default()).await })
    };
    let mut rx = h.shared.hooks.rebuild_reached.subscribe();
    rx.wait_for(|v| *v == Some(RebuildPoint::Copied)).await.unwrap();
    let before = h.size();
    h.put((100..110).map(|i| record(i, 150)).collect()).await.unwrap();
    let during: CollectionSize = h.size();
    assert_eq!(h.info().generation, 1);
    assert_eq!(during.records_total, before.records_total + 10);
    *h.shared.hooks.rebuild_pause.lock().unwrap() = None;
    sem.add_permits(1);
    rb.await.unwrap().unwrap();
    let info = h.info();
    assert_eq!(info.generation, 2);
    assert_eq!(info.size.tombstones, 0);
    assert_eq!(info.size.records, truth(&h).await.0);
    // 8 + 10 live records, plus the old warcinfo copied as an ordinary record.
    assert_eq!(info.size.records, 19);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn size_after_writer_reopen() {
    let td = TestDir::new();
    let repo = repo_with_dict(&td, &config()).await;
    let h = repo.create_collection("w", Codec::None).await.unwrap();
    h.put((1..=5).map(|i| record(i, 100)).collect()).await.unwrap();
    h.shared.hooks.drop_writer_next.store(true, Ordering::Release);
    h.put((6..=10).map(|i| record(i, 100)).collect()).await.unwrap();
    assert!(!h.shared.hooks.drop_writer_next.load(Ordering::Acquire));
    let s = h.size();
    assert_eq!(s.records, truth(&h).await.0);
    assert_eq!(s.records, 10);
    let dir = coll_dir(&repo, h.id());
    assert_eq!(s.segment_bytes, disk_seg_bytes(&dir, 1));
    assert_eq!(s.cdx_bytes, h.stats().cdx_len);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn size_tombstone_reopen_path() {
    let td = TestDir::new();
    let repo = repo_with_dict(&td, &config()).await;
    let h = repo.create_collection("t", Codec::None).await.unwrap();
    let put = h.put((1..=5).map(|i| record(i, 100)).collect()).await.unwrap();
    h.shared.hooks.drop_writer_next.store(true, Ordering::Release);
    h.delete(vec![put[2].clone()]).await.unwrap();
    assert!(!h.shared.hooks.drop_writer_next.load(Ordering::Acquire));
    let s = h.size();
    assert_eq!((s.tombstones, s.records_total, s.records), (1, 5, 4));
    assert_eq!(s.tombstoned_bytes, put[2].entry.raw_size);
    let (n, bytes) = truth(&h).await;
    assert_eq!(s.records, n);
    assert_eq!(s.stored_bytes - s.tombstoned_bytes, bytes);
    let dir = coll_dir(&repo, h.id());
    assert_eq!(s.segment_bytes, disk_seg_bytes(&dir, 1));
    assert_eq!(s.cdx_bytes, h.stats().cdx_len);
    repo.shutdown().await.unwrap();
}
}
