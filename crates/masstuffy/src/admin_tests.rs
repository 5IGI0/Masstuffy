//! Tests of `delete_collection`, `rename_collection` and record deletion (M21).

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use masstuffy_cdx::Entry;
use masstuffy_collection::{Codec, CollectionId, CommitError, TombstoneError};
use masstuffy_index::CollectionKey;
use tokio::sync::Semaphore;

use crate::collection::CollectionHandle;
use crate::config::Config;
use crate::error::{Error, ErrorKind};
use crate::repository::Repository;
use crate::scan::ScanOptions;
use crate::testutil::*;
use crate::types::{Located, MAX_DELETE_TARGETS, OpenReport, SkipReason};
use crate::writer::{DeletePoint, RebuildPoint};

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

async fn setup() -> (TestDir, Repository) {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &config())
        .await
        .unwrap();
    (td, repo)
}

fn coll_dir(repo: &Repository, id: CollectionId) -> PathBuf {
    repo.path().join("data/repository").join(id.to_string())
}

/// Entries of `data/repository` that are not collection directories.
fn trash_entries(repo: &Repository) -> Vec<String> {
    fs::read_dir(repo.path().join("data/repository"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.'))
        .collect()
}

fn uuid_dirs(repo: &Repository) -> usize {
    fs::read_dir(repo.path().join("data/repository"))
        .unwrap()
        .filter(|e| {
            !e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with('.')
        })
        .count()
}

fn rec(i: usize) -> masstuffy_warc::Record {
    record(i, 100 + i % 50)
}

/// Puts records `base..base + n`, one call; returns their locators.
async fn put_n(h: &CollectionHandle, base: usize, n: usize) -> Vec<Located> {
    h.put((base..base + n).map(rec).collect()).await.unwrap()
}

async fn reopen(repo: Repository, td: &TestDir) -> (Repository, OpenReport) {
    repo.shutdown().await.unwrap();
    drop(repo);
    Repository::open(td.repo()).await.unwrap()
}

fn is_unknown(e: &Error) -> bool {
    matches!(e, Error::UnknownCollection(_))
}

fn index_keys(repo: &Repository) -> Vec<CollectionKey> {
    repo.core().index().unwrap().collections().unwrap()
}

/// Record ids of the live non-warcinfo entries, sorted.
async fn live_ids(h: &CollectionHandle) -> Vec<String> {
    let mut v: Vec<String> = live_entries(h)
        .await
        .into_iter()
        .filter(|e| &*e.warc_type != "warcinfo")
        .map(|e| e.record_id.to_string())
        .collect();
    v.sort();
    v
}

fn ids_of(range: impl Iterator<Item = usize>) -> Vec<String> {
    let mut v: Vec<String> = range.map(id_str).collect();
    v.sort();
    v
}

fn pause_at(h: &CollectionHandle, p: RebuildPoint) -> Arc<Semaphore> {
    let s = Arc::new(Semaphore::new(0));
    *h.shared.hooks.rebuild_pause.lock().unwrap() = Some((p, Arc::clone(&s)));
    s
}

async fn reached(h: &CollectionHandle, p: RebuildPoint) {
    let mut rx = h.shared.hooks.rebuild_reached.subscribe();
    rx.wait_for(|v| *v == Some(p)).await.unwrap();
}

/// Waits (yielding) until `delete_collection` has set the `deleted` flag.
async fn wait_deleted(h: &CollectionHandle) {
    while !h.shared.is_deleted() {
        tokio::task::yield_now().await;
    }
}

/// True if `fut` is still pending after 100 ms.
async fn pending<F: std::future::Future + Unpin>(fut: &mut F) -> bool {
    tokio::time::timeout(Duration::from_millis(100), fut)
        .await
        .is_err()
}

fn crash_delete_at(h: &CollectionHandle, p: DeletePoint) {
    *h.shared.hooks.delete_crash.lock().unwrap() = Some(p);
}

async fn sync(repo: &Repository) {
    repo.sync_index().await.unwrap();
}

/// `index_dump` now equals `index_dump` after a full `rebuild_index` (I8).
async fn assert_index_matches_cdx(repo: &Repository, id: CollectionId) {
    sync(repo).await;
    let before = index_dump(repo, id).await;
    repo.rebuild_index().await.unwrap();
    assert_eq!(before, index_dump(repo, id).await);
}

// ---- delete_collection.

rt_test! {
async fn delete_collection_then_reopen() {
    let (td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    let id = h.id();
    let locs = put_n(&h, 0, 10).await;
    sync(&repo).await;
    let mut scan = h.scan(ScanOptions::default()).unwrap();

    let rep = repo.delete_collection("a").await.unwrap();
    assert!(rep.trash_removed && rep.index_dropped && !rep.slug_reserved);
    assert_eq!(&*rep.slug, "a");
    assert_eq!(rep.id, id);

    assert!(!coll_dir(&repo, id).exists());
    assert!(trash_entries(&repo).is_empty());
    assert!(is_unknown(&repo.collection("a").err().unwrap()));
    assert!(repo.collections().is_empty());

    assert!(is_unknown(&h.put(vec![rec(100)]).await.err().unwrap()));
    assert!(is_unknown(&scan.next_page().await.err().unwrap()));
    assert!(is_unknown(&h.scan(ScanOptions::default()).err().unwrap()));
    assert!(is_unknown(&repo.get(&locs[0]).await.err().unwrap()));
    assert!(is_unknown(&h.train_dictionary(Default::default()).await.err().unwrap()));
    assert!(is_unknown(&h.delete(vec![locs[1].clone()]).await.err().unwrap()));
    assert!(is_unknown(&h.delete_by_id(&id_str(1)).await.err().unwrap()));

    assert!(!index_keys(&repo).contains(&CollectionKey::new(id.as_u128())));
    assert!(index_dump(&repo, id).await.is_empty());

    let h2 = repo.create_collection("a", Codec::None).await.unwrap();
    assert_ne!(h2.id(), id);
    let (repo, report) = reopen(repo, &td).await;
    assert_eq!(report.loaded.len(), 1);
    assert_eq!(report.loaded[0].id, h2.id());
    assert_eq!(report.trash_removed, 0);
    assert!(report.skipped.is_empty());
    sync(&repo).await;
    assert_eq!(index_keys(&repo), vec![CollectionKey::new(h2.id().as_u128())]);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn delete_collection_by_id_and_twice() {
    let (_td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    put_n(&h, 0, 3).await;
    repo.delete_collection(h.id()).await.unwrap();
    assert!(is_unknown(&repo.delete_collection(h.id()).await.err().unwrap()));
    assert!(is_unknown(&repo.delete_collection("a").await.err().unwrap()));
    assert!(is_unknown(&repo.delete_collection("nope").await.err().unwrap()));
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn concurrent_double_delete() {
    let (_td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    put_n(&h, 0, 3).await;
    let id = h.id();
    let tasks: Vec<_> = (0..2)
        .map(|_| {
            let repo = repo.clone();
            tokio::spawn(async move { repo.delete_collection(id).await })
        })
        .collect();
    let mut ok = 0;
    let mut unknown = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(_) => ok += 1,
            Err(e) if is_unknown(&e) => unknown += 1,
            Err(e) => panic!("{e:?}"),
        }
    }
    assert_eq!((ok, unknown), (1, 1));
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn crash_after_trash_rename_manual() {
    let (td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    let id = h.id();
    put_n(&h, 0, 4).await;
    sync(&repo).await;
    let dir = coll_dir(&repo, id);
    let trash = dir.with_file_name(format!(".trash-{id}-0-0"));
    repo.shutdown().await.unwrap();
    drop((repo, h));
    fs::rename(&dir, &trash).unwrap();
    let (repo, report) = Repository::open(td.repo()).await.unwrap();
    assert_eq!(report.trash_removed, 1);
    assert!(report.loaded.is_empty() && report.skipped.is_empty());
    assert!(!trash.exists());
    sync(&repo).await;
    assert!(index_keys(&repo).is_empty());
    repo.create_collection("a", Codec::None).await.unwrap();
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn crash_hook_after_trash() {
    let (td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    put_n(&h, 0, 4).await;
    sync(&repo).await;
    crash_delete_at(&h, DeletePoint::Trashed);
    let e = repo.delete_collection("a").await.err().unwrap();
    assert!(matches!(e, Error::Internal("injected delete crash")), "{e:?}");
    let (repo, report) = reopen(repo, &td).await;
    assert!(report.loaded.is_empty() && report.skipped.is_empty());
    assert!(repo.collection("a").is_err());
    sync(&repo).await;
    assert!(index_keys(&repo).is_empty());
    repo.create_collection("a", Codec::None).await.unwrap();
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn crash_hook_before_trash() {
    let (td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    let id = h.id();
    put_n(&h, 0, 6).await;
    sync(&repo).await;
    crash_delete_at(&h, DeletePoint::Stopped);
    let e = repo.delete_collection("a").await.err().unwrap();
    assert!(matches!(e, Error::Internal("injected delete crash")), "{e:?}");
    let (repo, report) = reopen(repo, &td).await;
    assert_eq!(report.loaded.len(), 1);
    assert_eq!(report.trash_removed, 0);
    let h = repo.collection("a").unwrap();
    assert_eq!(h.id(), id);
    assert_eq!(live_ids(&h).await, ids_of(0..6));
    sync(&repo).await;
    assert_eq!(index_dump(&repo, id).await.len(), 7);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn delete_with_puts_in_flight() {
    let (_td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    let hold = Arc::new(Semaphore::new(0));
    *h.shared.hooks.after_prepare.lock().unwrap() = Some(Arc::clone(&hold));
    let put = {
        let h = h.clone();
        tokio::spawn(async move { h.put(vec![rec(1)]).await })
    };
    let mut rx = h.shared.hooks.after_prepare_reached.subscribe();
    rx.wait_for(|n| *n == 1).await.unwrap();
    let mut del = {
        let repo = repo.clone();
        tokio::spawn(async move { repo.delete_collection("a").await })
    };
    assert!(pending(&mut del).await);
    hold.add_permits(1);
    assert_eq!(put.await.unwrap().unwrap().len(), 1);
    del.await.unwrap().unwrap();
    assert!(is_unknown(&h.put(vec![rec(2)]).await.err().unwrap()));
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn delete_with_concurrent_puts() {
    let (td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    // Some acknowledged data before the delete races the concurrent puts (no polling).
    for k in 0..5usize {
        h.put(vec![rec(10_000 + k)]).await.unwrap();
    }
    let mut puts = Vec::new();
    for t in 0..8usize {
        let h = h.clone();
        puts.push(tokio::spawn(async move {
            let mut ok = 0;
            for k in 0..20usize {
                match h.put(vec![rec(t * 100 + k)]).await {
                    Ok(_) => ok += 1,
                    Err(e) if is_unknown(&e) => {}
                    Err(e) => panic!("{e:?}"),
                }
            }
            ok
        }));
    }
    let del = {
        let repo = repo.clone();
        tokio::spawn(async move { repo.delete_collection("a").await })
    };
    let mut total = 0;
    for p in puts {
        total += p.await.unwrap();
    }
    del.await.unwrap().unwrap();
    assert!(total <= 160);
    let (repo, report) = reopen(repo, &td).await;
    assert!(report.loaded.is_empty());
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn delete_during_rebuild() {
    let (td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    let id = h.id();
    put_n(&h, 0, 10).await;
    let pause = pause_at(&h, RebuildPoint::Started);
    let rb = {
        let h = h.clone();
        tokio::spawn(async move { h.rebuild(Codec::None, Default::default()).await })
    };
    reached(&h, RebuildPoint::Started).await;
    let del = {
        let repo = repo.clone();
        tokio::spawn(async move { repo.delete_collection("a").await })
    };
    wait_deleted(&h).await;
    pause.add_permits(1);
    let e = rb.await.unwrap().err().unwrap();
    assert!(is_unknown(&e), "{e:?}");
    del.await.unwrap().unwrap();
    assert!(!coll_dir(&repo, id).exists());
    let (repo, report) = reopen(repo, &td).await;
    assert!(report.loaded.is_empty());
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rebuild_after_delete_rejected() {
    let (_td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    repo.delete_collection("a").await.unwrap();
    let e = h.rebuild(Codec::None, Default::default()).await.err().unwrap();
    assert!(is_unknown(&e), "{e:?}");
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn delete_vs_shutdown() {
    let (td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    put_n(&h, 0, 5).await;
    let del = {
        let repo = repo.clone();
        tokio::spawn(async move { repo.delete_collection("a").await })
    };
    let sd = {
        let repo = repo.clone();
        tokio::spawn(async move { repo.shutdown().await })
    };
    let deleted = match del.await.unwrap() {
        Ok(_) => true,
        Err(Error::ShuttingDown) => false,
        Err(e) => panic!("{e:?}"),
    };
    sd.await.unwrap().unwrap();
    drop((repo, h));
    let (repo, report) = Repository::open(td.repo()).await.unwrap();
    assert!(report.trash_removed <= 1);
    if deleted {
        assert!(report.loaded.is_empty());
    } else {
        assert_eq!(report.loaded.len(), 1);
        let h = repo.collection("a").unwrap();
        assert_eq!(live_ids(&h).await, ids_of(0..5));
    }
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn delete_index_failure_falls_back_to_gc() {
    let (_td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    let id = h.id();
    put_n(&h, 0, 5).await;
    sync(&repo).await;
    index_hooks(&repo)
        .fail_applies
        .store(1, std::sync::atomic::Ordering::Release);
    let rep = repo.delete_collection("a").await.unwrap();
    assert!(!rep.index_dropped);
    sync(&repo).await;
    assert!(!index_keys(&repo).contains(&CollectionKey::new(id.as_u128())));
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn forget_removes_track_and_rows() {
    let (_td, repo) = setup().await;
    let a = repo.create_collection("a", Codec::None).await.unwrap();
    let b = repo.create_collection("b", Codec::None).await.unwrap();
    put_n(&a, 0, 5).await;
    put_n(&b, 100, 5).await;
    sync(&repo).await;
    repo.delete_collection("a").await.unwrap();
    sync(&repo).await;
    assert_eq!(index_keys(&repo), vec![CollectionKey::new(b.id().as_u128())]);
    put_n(&b, 200, 3).await;
    sync(&repo).await;
    assert_eq!(index_dump(&repo, b.id()).await.len(), 1 + 5 + 3);
    repo.shutdown().await.unwrap();
}
}

// ---- rename and slug uniqueness.

rt_test! {
async fn rename_and_reopen() {
    let (td, repo) = setup().await;
    let a = repo.create_collection("a", Codec::None).await.unwrap();
    let id = a.id();
    put_n(&a, 0, 5).await;
    let renamed = repo.rename_collection("a", "b").await.unwrap();
    assert_eq!(&*renamed.slug(), "b");
    assert_eq!(&*a.slug(), "b");
    assert_eq!(&*a.info().slug, "b");
    assert!(is_unknown(&repo.collection("a").err().unwrap()));
    assert_eq!(repo.collection("b").unwrap().id(), id);
    let l = put_n(&a, 5, 1).await;
    assert_eq!(repo.get(&l[0]).await.unwrap(), rec(5).as_bytes());
    let m = fs::read_to_string(coll_dir(&repo, id).join("manifest.json")).unwrap();
    assert!(m.contains("\"slug\":\"b\""), "{m}");
    let a2 = repo.create_collection("a", Codec::None).await.unwrap();
    assert_ne!(a2.id(), id);
    let (repo, report) = reopen(repo, &td).await;
    assert_eq!(report.loaded.len(), 2);
    let b = repo.collection("b").unwrap();
    assert_eq!(b.id(), id);
    assert_eq!(live_entries(&b).await.len(), 7);
    assert_eq!(repo.collection("a").unwrap().id(), a2.id());
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rename_errors() {
    let td = TestDir::new();
    let repo = repo_with_dict(&td, &config()).await;
    let a = repo.create_collection("a", Codec::None).await.unwrap();
    repo.create_collection("b", Codec::None).await.unwrap();
    repo.create_collection("z", zstd_codec()).await.unwrap();
    let manifest = coll_dir(&repo, a.id()).join("manifest.json");
    let before = fs::read(&manifest).unwrap();

    let e = repo.rename_collection("a", "b").await.err().unwrap();
    assert!(matches!(e, Error::SlugTaken { .. }), "{e:?}");
    assert_eq!(fs::read(&manifest).unwrap(), before);
    let e = repo.rename_collection("a", "B!").await.err().unwrap();
    assert!(matches!(e, Error::InvalidSlug { .. }), "{e:?}");
    let e = repo.rename_collection("nope", "c").await.err().unwrap();
    assert!(is_unknown(&e), "{e:?}");
    repo.rename_collection("a", "a").await.unwrap();
    assert_eq!(fs::read(&manifest).unwrap(), before);

    // A collection skipped at open keeps its slug reserved.
    repo.shutdown().await.unwrap();
    drop((repo, a));
    fs::remove_file(
        td.repo().join("data/dict/zstd").join(format!("test.{}.zstdict", dict_id())),
    )
    .unwrap();
    let (repo, report) = Repository::open(td.repo()).await.unwrap();
    assert_eq!(report.skipped.len(), 1);
    assert!(matches!(report.skipped[0].reason, SkipReason::Open(_)));
    let e = repo.rename_collection("a", "z").await.err().unwrap();
    assert!(matches!(e, Error::SlugTaken { .. }), "{e:?}");
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rename_then_rebuild() {
    let (td, repo) = setup().await;
    let a = repo.create_collection("a", Codec::None).await.unwrap();
    put_n(&a, 0, 5).await;
    repo.rename_collection("a", "b").await.unwrap();
    a.rebuild(Codec::None, Default::default()).await.unwrap();
    assert_eq!(a.info().generation, 2);
    let wi = live_entries(&a).await.remove(0);
    assert_eq!(&*wi.warc_type, "warcinfo");
    let bytes = repo
        .get(&Located { collection: a.id(), generation: 2, entry: wi })
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("\"slug\":\"b\""));
    let (repo, _) = reopen(repo, &td).await;
    repo.collection("b").unwrap();
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rename_waits_for_rebuild() {
    let (td, repo) = setup().await;
    let a = repo.create_collection("a", Codec::None).await.unwrap();
    put_n(&a, 0, 5).await;
    let pause = pause_at(&a, RebuildPoint::Started);
    let rb = {
        let a = a.clone();
        tokio::spawn(async move { a.rebuild(Codec::None, Default::default()).await })
    };
    reached(&a, RebuildPoint::Started).await;
    let mut rn = {
        let repo = repo.clone();
        tokio::spawn(async move { repo.rename_collection("a", "b").await })
    };
    assert!(pending(&mut rn).await);
    assert_eq!(&*a.slug(), "a");
    pause.add_permits(1);
    rb.await.unwrap().unwrap();
    rn.await.unwrap().unwrap();
    assert_eq!(&*a.slug(), "b");
    let (repo, _) = reopen(repo, &td).await;
    repo.collection("b").unwrap();
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rename_by_id() {
    let (_td, repo) = setup().await;
    let a = repo.create_collection("a", Codec::None).await.unwrap();
    repo.rename_collection(a.id(), "z").await.unwrap();
    assert_eq!(repo.collection("z").unwrap().id(), a.id());
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn concurrent_creates_same_slug() {
    let (td, repo) = setup().await;
    let tasks: Vec<_> = (0..32)
        .map(|_| {
            let repo = repo.clone();
            tokio::spawn(async move { repo.create_collection("same", Codec::None).await })
        })
        .collect();
    let (mut ok, mut taken) = (0, 0);
    for t in tasks {
        match t.await.unwrap() {
            Ok(_) => ok += 1,
            Err(Error::SlugTaken { .. }) => taken += 1,
            Err(e) => panic!("{e:?}"),
        }
    }
    assert_eq!((ok, taken), (1, 31));
    assert_eq!(repo.collections().len(), 1);
    assert_eq!(uuid_dirs(&repo), 1);
    let (repo, report) = reopen(repo, &td).await;
    assert_eq!(report.loaded.len(), 1);
    assert!(report.skipped.is_empty());
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn concurrent_create_vs_rename() {
    let (td, repo) = setup().await;
    let mut expected = 0;
    for i in 0..20 {
        let (s, t) = (format!("s{i}"), format!("t{i}"));
        repo.create_collection(&s, Codec::None).await.unwrap();
        let c = {
            let (repo, t) = (repo.clone(), t.clone());
            tokio::spawn(async move { repo.create_collection(&t, Codec::None).await.map(|_| ()) })
        };
        let r = {
            let (repo, s, t) = (repo.clone(), s.clone(), t.clone());
            tokio::spawn(async move { repo.rename_collection(&*s, &t).await.map(|_| ()) })
        };
        let (c, r) = (c.await.unwrap(), r.await.unwrap());
        match (&c, &r) {
            (Ok(()), Err(Error::SlugTaken { .. })) => expected += 2,
            (Err(Error::SlugTaken { .. }), Ok(())) => expected += 1,
            other => panic!("{other:?}"),
        }
        repo.collection(&t).unwrap();
    }
    assert_eq!(repo.collections().len(), expected);
    let (repo, report) = reopen(repo, &td).await;
    assert!(
        !report
            .skipped
            .iter()
            .any(|s| matches!(s.reason, SkipReason::DuplicateSlug { .. }))
    );
    assert!(report.skipped.is_empty());
    assert_eq!(report.loaded.len(), expected);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn concurrent_renames_to_same_slug() {
    let (td, repo) = setup().await;
    repo.create_collection("a", Codec::None).await.unwrap();
    repo.create_collection("b", Codec::None).await.unwrap();
    let tasks: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|s| {
            let repo = repo.clone();
            tokio::spawn(async move { repo.rename_collection(s, "c").await.map(|_| ()) })
        })
        .collect();
    let (mut ok, mut taken) = (0, 0);
    for t in tasks {
        match t.await.unwrap() {
            Ok(()) => ok += 1,
            Err(Error::SlugTaken { .. }) => taken += 1,
            Err(e) => panic!("{e:?}"),
        }
    }
    assert_eq!((ok, taken), (1, 1));
    let (repo, report) = reopen(repo, &td).await;
    assert_eq!(report.loaded.len(), 2);
    assert!(report.skipped.is_empty());
    repo.collection("c").unwrap();
    repo.shutdown().await.unwrap();
}
}

// ---- Record deletion.

rt_test! {
async fn delete_record_basic() {
    let (_td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    let id = h.id();
    let locs = put_n(&h, 1, 5).await;
    sync(&repo).await;
    assert_eq!(h.delete(vec![locs[1].clone()]).await.unwrap(), 1);
    assert_eq!(live_ids(&h).await, ids_of([1, 3, 4, 5].into_iter()));
    sync(&repo).await;
    assert!(repo.find_by_id(&id_str(2), Some(id)).await.unwrap().is_none());
    assert!(repo.get_by_id(&id_str(2), Some(id)).await.unwrap().is_none());
    assert!(h.find_by_url(&url(2), None).await.unwrap().is_none());
    // Logical deletion: the locator still reads (documented).
    assert_eq!(repo.get(&locs[1]).await.unwrap(), rec(2).as_bytes());
    for i in [1, 3, 4, 5] {
        assert!(repo.find_by_id(&id_str(i), Some(id)).await.unwrap().is_some());
    }
    assert_index_matches_cdx(&repo, id).await;
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn delete_twice_and_dedupe() {
    let (_td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    let locs = put_n(&h, 1, 5).await;
    let l2 = locs[1].clone();
    let len0 = h.stats().cdx_len;
    assert_eq!(h.delete(vec![l2.clone(), l2.clone()]).await.unwrap(), 1);
    let len1 = h.stats().cdx_len;
    assert_eq!(h.delete(vec![l2.clone()]).await.unwrap(), 1);
    let len2 = h.stats().cdx_len;
    assert!(len1 > len0);
    assert_eq!(len1 - len0, len2 - len1);
    assert_eq!(live_ids(&h).await, ids_of([1, 3, 4, 5].into_iter()));
    sync(&repo).await;
    assert_index_matches_cdx(&repo, h.id()).await;
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn delete_errors() {
    let (_td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    let locs = put_n(&h, 1, 3).await;
    let good = locs[0].clone();
    let wi = Located {
        collection: h.id(),
        generation: 1,
        entry: live_entries(&h).await.remove(0),
    };
    let len0 = h.stats().cdx_len;

    assert_eq!(h.delete(vec![]).await.unwrap(), 0);
    let e = h.delete(vec![wi.clone()]).await.err().unwrap();
    assert!(
        matches!(e, Error::Tombstone { index: 0, source: TombstoneError::Warcinfo }),
        "{e:?}"
    );
    assert_eq!(e.kind(), ErrorKind::Invalid);
    let mut other = good.clone();
    other.collection = CollectionId::from_u128(42);
    let e = h.delete(vec![other]).await.err().unwrap();
    assert!(matches!(e, Error::InvalidOptions(_)), "{e:?}");
    let mut past = good.clone();
    past.entry.offset += 1 << 30;
    let e = h.delete(vec![past]).await.err().unwrap();
    assert!(
        matches!(e, Error::Tombstone { index: 0, source: TombstoneError::PastEnd { .. } }),
        "{e:?}"
    );
    let e = h
        .delete(vec![good.clone(); MAX_DELETE_TARGETS + 1])
        .await
        .err()
        .unwrap();
    assert!(matches!(e, Error::InvalidOptions(_)), "{e:?}");
    let e = h.delete(vec![good.clone(), wi]).await.err().unwrap();
    assert!(matches!(e, Error::Tombstone { index: 1, .. }), "{e:?}");
    assert_eq!(h.stats().cdx_len, len0);
    assert_eq!(live_ids(&h).await, ids_of(1..4));
    repo.shutdown().await.unwrap();
}
}

#[test]
fn tombstone_not_committed_is_invalid() {
    let e = Error::Commit {
        collection: CollectionId::from_u128(1),
        source: Arc::new(CommitError::TombstoneNotCommitted {
            index: 0,
            segment: 1,
            end: 10,
            committed: 5,
        }),
    };
    assert_eq!(e.kind(), ErrorKind::Invalid);
    let e = Error::Commit {
        collection: CollectionId::from_u128(1),
        source: Arc::new(CommitError::Poisoned),
    };
    assert_eq!(e.kind(), ErrorKind::Io);
}

rt_test! {
async fn delete_not_committed_target() {
    let (_td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    put_n(&h, 1, 2).await;
    // An entry that points past the committed end of segment 1 but within the file: the file is
    // only as long as what was committed, so craft it from a real one and grow the segment.
    let live = live_entries(&h).await;
    let last = live.last().unwrap().clone();
    let seg = coll_dir(&repo, h.id()).join("seg.1.1.warc");
    let len = fs::metadata(&seg).unwrap().len();
    {
        use std::io::Write;
        let mut f = fs::OpenOptions::new().append(true).open(&seg).unwrap();
        f.write_all(&[b'x'; 64]).unwrap();
        f.sync_all().unwrap();
    }
    let mut e: Entry<'static> = last;
    e.offset = len;
    e.raw_size = 64;
    let t = Located { collection: h.id(), generation: 1, entry: e };
    let err = h.delete(vec![t]).await.err().unwrap();
    assert!(
        matches!(&err, Error::Commit { source, .. } if matches!(**source, CommitError::TombstoneNotCommitted { .. })),
        "{err:?}"
    );
    assert_eq!(err.kind(), ErrorKind::Invalid);
    // The writer is still usable.
    put_n(&h, 10, 1).await;
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn delete_stale_after_rebuild() {
    let (_td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    let locs = put_n(&h, 1, 3).await;
    h.rebuild(Codec::None, Default::default()).await.unwrap();
    let e = h.delete(vec![locs[0].clone()]).await.err().unwrap();
    assert!(matches!(e, Error::StaleLocator { expected: 2, got: 1 }), "{e:?}");
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn delete_waits_for_rebuild() {
    let (_td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    let locs = put_n(&h, 1, 10).await;
    let pause = pause_at(&h, RebuildPoint::Started);
    let rb = {
        let h = h.clone();
        tokio::spawn(async move { h.rebuild(Codec::None, Default::default()).await })
    };
    reached(&h, RebuildPoint::Started).await;
    let mut del = {
        let h = h.clone();
        let l3 = locs[2].clone();
        tokio::spawn(async move { h.delete(vec![l3]).await })
    };
    assert!(pending(&mut del).await);
    pause.add_permits(1);
    rb.await.unwrap().unwrap();
    *h.shared.hooks.rebuild_pause.lock().unwrap() = None;
    let e = del.await.unwrap().err().unwrap();
    assert!(matches!(e, Error::StaleLocator { expected: 2, got: 1 }), "{e:?}");
    sync(&repo).await;
    let l = h.delete_by_id(&id_str(3)).await.unwrap().unwrap();
    assert_eq!(l.generation, 2);
    assert_eq!(live_ids(&h).await, ids_of((1..11).filter(|&i| i != 3)));
    h.rebuild(Codec::None, Default::default()).await.unwrap();
    assert_eq!(live_ids(&h).await, ids_of((1..11).filter(|&i| i != 3)));
    // 9 records and 3 warcinfos (generations 1 to 3).
    assert_eq!(live_entries(&h).await.len(), 12);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rebuild_after_deletes_drops_records() {
    let (_td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    let locs = put_n(&h, 1, 10).await;
    assert_eq!(
        h.delete(vec![locs[1].clone(), locs[4].clone(), locs[7].clone()])
            .await
            .unwrap(),
        3
    );
    let rep = h.rebuild(Codec::None, Default::default()).await.unwrap();
    assert_eq!(rep.copy.dropped_tombstoned, 3);
    let gone = [2, 5, 8];
    assert_eq!(live_ids(&h).await, ids_of((1..11).filter(|i| !gone.contains(i))));
    sync(&repo).await;
    for i in gone {
        assert!(repo.find_by_id(&id_str(i), Some(h.id())).await.unwrap().is_none());
    }
    assert_index_matches_cdx(&repo, h.id()).await;
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn deletes_survive_reopen() {
    let (td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    let id = h.id();
    let locs = put_n(&h, 1, 6).await;
    h.delete(vec![locs[0].clone(), locs[3].clone()]).await.unwrap();
    let (repo, _) = reopen(repo, &td).await;
    let h = repo.collection("a").unwrap();
    assert_eq!(live_ids(&h).await, ids_of([2, 3, 5, 6].into_iter()));
    assert_index_matches_cdx(&repo, id).await;
    assert!(repo.find_by_id(&id_str(1), Some(id)).await.unwrap().is_none());
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn concurrent_puts_and_deletes() {
    let (_td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    let pre = put_n(&h, 0, 50).await;
    let mut tasks = Vec::new();
    for t in 0..4usize {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            for k in 0..25usize {
                h.put(vec![rec(1000 + t * 100 + k)]).await.unwrap();
            }
        }));
    }
    for t in 0..4usize {
        let h = h.clone();
        let targets: Vec<Located> = pre[t * 10..t * 10 + 10].to_vec();
        tasks.push(tokio::spawn(async move {
            h.delete(targets).await.unwrap();
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let mut want: Vec<usize> = (40..50).collect();
    want.extend((0..4usize).flat_map(|t| (0..25usize).map(move |k| 1000 + t * 100 + k)));
    assert_eq!(live_ids(&h).await, ids_of(want.into_iter()));
    assert_index_matches_cdx(&repo, h.id()).await;
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn delete_by_id_duplicates() {
    let (_td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    let first = h.put(vec![rec(7)]).await.unwrap().remove(0);
    let second = h.put(vec![rec(7)]).await.unwrap().remove(0);
    sync(&repo).await;
    assert_eq!(h.delete_by_id(&id_str(7)).await.unwrap(), Some(second));
    sync(&repo).await;
    assert_eq!(h.delete_by_id(&id_str(7)).await.unwrap(), Some(first));
    sync(&repo).await;
    assert_eq!(h.delete_by_id(&id_str(7)).await.unwrap(), None);
    assert!(matches!(
        h.delete_by_id("").await.err().unwrap(),
        Error::InvalidQuery(_)
    ));
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn delete_reopen_path() {
    let (_td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    let locs = put_n(&h, 1, 4).await;
    h.shared
        .hooks
        .drop_writer_next
        .store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(h.delete(vec![locs[0].clone()]).await.unwrap(), 1);
    assert_eq!(live_ids(&h).await, ids_of(2..5));
    put_n(&h, 10, 1).await;
    sync(&repo).await;
    assert_index_matches_cdx(&repo, h.id()).await;
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn delete_vs_shutdown_records() {
    let (td, repo) = setup().await;
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    let locs = put_n(&h, 1, 3).await;
    let del = {
        let h = h.clone();
        let l = locs[0].clone();
        tokio::spawn(async move { h.delete(vec![l]).await })
    };
    let sd = {
        let repo = repo.clone();
        tokio::spawn(async move { repo.shutdown().await })
    };
    let deleted = match del.await.unwrap() {
        Ok(1) => true,
        Err(Error::ShuttingDown) => false,
        other => panic!("{other:?}"),
    };
    sd.await.unwrap().unwrap();
    drop((repo, h));
    let (repo, _) = Repository::open(td.repo()).await.unwrap();
    let h = repo.collection("a").unwrap();
    let want = if deleted { ids_of(2..4) } else { ids_of(1..4) };
    assert_eq!(live_ids(&h).await, want);
    repo.shutdown().await.unwrap();
}
}
