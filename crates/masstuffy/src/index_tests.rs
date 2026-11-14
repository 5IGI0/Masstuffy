//! Secondary-index wiring tests: tailing, catch-up, re-index, `sync_index`, `rebuild_index`,
//! purge, shutdown.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use masstuffy_cdx::Entry;
use masstuffy_collection::{Codec, Collection, CollectionId, RecoveryOptions, RepairPolicy};
use masstuffy_index::{Checkpoint, CollectionKey, CollectionState};
#[cfg(feature = "redb")]
use masstuffy_index::{Op, SecondaryIndex};
use tokio::sync::Semaphore;

use crate::collection::CollectionHandle;
use crate::config::{Config, Limits};
use crate::error::Error;
use crate::repository::Repository;
use crate::testutil::*;

macro_rules! rt_test {
    ($(#[$m:meta])* async fn $name:ident() $body:block) => {
        $(#[$m])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            bounded(async $body).await
        }
    };
}

fn key(h: &CollectionHandle) -> CollectionKey {
    CollectionKey::new(h.id().as_u128())
}

fn coll_dir(dir: &Path, id: CollectionId) -> PathBuf {
    dir.join("data/repository").join(id.to_string())
}

fn index_state(repo: &Repository, h: &CollectionHandle) -> CollectionState {
    repo.core().index().unwrap().state(key(h)).unwrap().unwrap()
}

/// The index agrees with the CDX of `h`.
async fn assert_i8(repo: &Repository, h: &CollectionHandle) {
    let want = live_entries(h).await;
    assert_eq!(index_dump(repo, h.id()).await, want);
}

async fn put_n(h: &CollectionHandle, base: usize, n: usize) {
    let recs = (base..base + n).map(|i| record(i, 40)).collect();
    h.put(recs).await.unwrap();
}

fn arm(repo: &Repository) -> Arc<Semaphore> {
    let s = Arc::new(Semaphore::new(0));
    *index_hooks(repo).hold.lock().unwrap() = Some(Arc::clone(&s));
    s
}

fn disarm(repo: &Repository) {
    *index_hooks(repo).hold.lock().unwrap() = None;
}

#[cfg(feature = "redb")]
fn bogus(e: &Entry<'static>) -> Entry<'static> {
    Entry {
        segment: 99,
        ..e.clone()
    }
}

#[cfg(feature = "redb")]
mod direct {
    use super::*;
    use masstuffy_index_redb::{Options, RedbIndex};

    pub(super) fn open(dir: &Path) -> RedbIndex {
        RedbIndex::open(crate::index::index_path(dir), &Options::default()).unwrap()
    }
}

rt_test! {
async fn new_collection_indexes_warcinfo() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    repo.sync_index().await.unwrap();
    assert_eq!(index_dump(&repo, h.id()).await.len(), 1);
    assert_i8(&repo, &h).await;
    let st = index_state(&repo, &h);
    assert_eq!(st.active_generation, Some(1));
    assert_eq!(
        st.checkpoint,
        Some(Checkpoint { generation: 1, cdx_offset: h.stats().cdx_len })
    );
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn puts_are_indexed_after_sync() {
    let td = TestDir::new();
    let repo = repo_with_dict(&td, &small_config()).await;
    let h = repo.create_collection("z", zstd_codec()).await.unwrap();
    let mut joins = Vec::new();
    for t in 0..3 {
        let h = h.clone();
        joins.push(tokio::spawn(async move {
            for j in 0..20 {
                put_n(&h, t * 1000 + j, 1).await;
            }
        }));
    }
    for j in joins {
        j.await.unwrap();
    }
    repo.sync_index().await.unwrap();
    assert_eq!(index_dump(&repo, h.id()).await.len(), 61);
    assert_i8(&repo, &h).await;
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn batches_across_collections() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let mut hs = Vec::new();
    for i in 0..4 {
        hs.push(repo.create_collection(&format!("c{i}"), Codec::None).await.unwrap());
    }
    repo.sync_index().await.unwrap();
    let sem = arm(&repo);
    for (i, h) in hs.iter().enumerate() {
        // Puts never wait for the index.
        put_n(h, i, 1).await;
    }
    let hooks = index_hooks(&repo);
    let a0 = hooks.applies.load(Ordering::Acquire);
    disarm(&repo);
    sem.add_permits(1);
    repo.sync_index().await.unwrap();
    assert_eq!(hooks.applies.load(Ordering::Acquire) - a0, 1);
    for h in &hs {
        assert_i8(&repo, h).await;
    }
    repo.shutdown().await.unwrap();
}
}

#[cfg(feature = "redb")]
rt_test! {
async fn kill_between_cdx_fsync_and_index_apply_catches_up() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    put_n(&h, 0, 20).await;
    repo.sync_index().await.unwrap();
    let sem = arm(&repo);
    put_n(&h, 100, 30).await; // acknowledged: the CDX is fsynced
    let cdx_len = h.stats().cdx_len;
    let id = h.id();
    index_hooks(&repo).abandon.store(true, Ordering::Release);
    sem.add_permits(1);
    repo.shutdown().await.unwrap();
    drop((repo, h));
    {
        let ix = direct::open(&td.repo());
        assert_eq!(dump_index(&ix, id).len(), 21);
        let cp = ix.state(CollectionKey::new(id.as_u128())).unwrap().unwrap().checkpoint.unwrap();
        assert!(cp.cdx_offset < cdx_len);
    }
    let (repo, _) = Repository::open(td.repo()).await.unwrap();
    repo.sync_index().await.unwrap();
    let h = repo.collection("a").unwrap();
    assert_eq!(index_dump(&repo, id).await.len(), 51);
    assert_i8(&repo, &h).await;
    assert_eq!(index_hooks(&repo).reindexes.load(Ordering::Acquire), 0);
    repo.shutdown().await.unwrap();
}
}

/// Shuts down a repository with one collection "a" holding `n` puts; returns what tests need.
#[cfg(feature = "redb")]
async fn shut_one(td: &TestDir, n: usize) -> (CollectionId, u64, Entry<'static>) {
    let repo = Repository::init(td.repo()).await.unwrap();
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    put_n(&h, 0, n).await;
    repo.sync_index().await.unwrap();
    let e = live_entries(&h).await[1].clone();
    let r = (h.id(), h.stats().cdx_len, e);
    repo.shutdown().await.unwrap();
    r
}

#[cfg(feature = "redb")]
async fn reopen_and_check(td: &TestDir, id: CollectionId, min_reindexes: u64) {
    let (repo, _) = Repository::open(td.repo()).await.unwrap();
    repo.sync_index().await.unwrap();
    let h = repo.collection_by_id(id).unwrap();
    assert_i8(&repo, &h).await;
    assert!(index_hooks(&repo).reindexes.load(Ordering::Acquire) >= min_reindexes);
    repo.shutdown().await.unwrap();
}

#[cfg(feature = "redb")]
rt_test! {
async fn checkpoint_beyond_cdx_reindexes() {
    let td = TestDir::new();
    let (id, len, e) = shut_one(&td, 10).await;
    {
        let ix = direct::open(&td.repo());
        let k = CollectionKey::new(id.as_u128());
        ix.apply(&[
            Op::Insert { collection: k, generation: 1, entry: bogus(&e) },
            Op::Checkpoint { collection: k, checkpoint: Checkpoint { generation: 1, cdx_offset: len + 1000 } },
        ])
        .unwrap();
        assert_eq!(dump_index(&ix, id).len(), 12);
    }
    reopen_and_check(&td, id, 1).await;
}
}

#[cfg(feature = "redb")]
rt_test! {
async fn misaligned_checkpoint_reindexes() {
    let td = TestDir::new();
    let (id, len, e) = shut_one(&td, 10).await;
    {
        let ix = direct::open(&td.repo());
        let k = CollectionKey::new(id.as_u128());
        ix.apply(&[
            Op::Insert { collection: k, generation: 1, entry: bogus(&e) },
            Op::Checkpoint { collection: k, checkpoint: Checkpoint { generation: 1, cdx_offset: len - 1 } },
        ])
        .unwrap();
    }
    reopen_and_check(&td, id, 1).await;
}
}

rt_test! {
async fn repair_truncation_reindexes_even_after_new_puts() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    for b in 0..3 {
        put_n(&h, b * 10, 10).await;
    }
    repo.sync_index().await.unwrap();
    let id = h.id();
    let dir = coll_dir(&td.repo(), id);
    let resolver = Arc::clone(&h.core.resolver);
    let opts = Limits::new(repo.config()).collection_opts;
    repo.shutdown().await.unwrap();
    drop((repo, h));
    // Corrupt the second line after the header (the first put record): not the last line.
    let cdx = dir.join("index.1.cdx");
    let mut bytes = fs::read(&cdx).unwrap();
    let nl: Vec<usize> = bytes.iter().enumerate().filter(|(_, b)| **b == b'\n').map(|(i, _)| i).collect();
    bytes[nl[1] + 3] ^= 1;
    fs::write(&cdx, bytes).unwrap();
    let recovery = RecoveryOptions {
        repair: RepairPolicy::TruncateAtCorruption,
        full_scan: true,
        ..RecoveryOptions::default()
    };
    let (c, report) = Collection::open(&dir, &opts, &recovery, &*resolver).unwrap();
    assert!(report.repair.is_some());
    drop(c);
    let (repo, _) = Repository::open(td.repo()).await.unwrap();
    let h = repo.collection_by_id(id).unwrap();
    // Immediately: the CDX grows past the stale checkpoint before the first pass.
    put_n(&h, 1000, 40).await;
    repo.sync_index().await.unwrap();
    assert_i8(&repo, &h).await;
    assert!(index_hooks(&repo).reindexes.load(Ordering::Acquire) >= 1);
    repo.shutdown().await.unwrap();
}
}

#[cfg(feature = "redb")]
rt_test! {
async fn index_file_garbage_is_recreated() {
    let td = TestDir::new();
    let (id, _, _) = shut_one(&td, 10).await;
    fs::write(crate::index::index_path(&td.repo()), vec![0x5a; 4096]).unwrap();
    let (repo, report) = Repository::open(td.repo()).await.unwrap();
    assert!(report.index_recreated);
    repo.sync_index().await.unwrap();
    let h = repo.collection_by_id(id).unwrap();
    assert_i8(&repo, &h).await;
    repo.shutdown().await.unwrap();
    let (repo, report) = Repository::open(td.repo()).await.unwrap();
    assert!(!report.index_recreated);
    repo.shutdown().await.unwrap();
}
}

#[cfg(feature = "redb")]
rt_test! {
async fn index_already_open_fails_open_cleanly() {
    let td = TestDir::new();
    shut_one(&td, 1).await;
    let ix = direct::open(&td.repo());
    let e = Repository::open(td.repo()).await.err().unwrap();
    assert!(matches!(e, Error::IndexOpen { .. }), "{e:?}");
    drop(ix);
    // The failed open released the directory lock.
    let (repo, _) = Repository::open(td.repo()).await.unwrap();
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn i8_incremental_equals_rebuilt() {
    let td = TestDir::new();
    let repo = repo_with_dict(&td, &small_config()).await;
    let a = repo.create_collection("a", Codec::None).await.unwrap();
    let b = repo.create_collection("b", zstd_codec()).await.unwrap();
    let mut joins = Vec::new();
    for t in 0..4 {
        for h in [a.clone(), b.clone()] {
            joins.push(tokio::spawn(async move {
                for j in 0..10 {
                    put_n(&h, t * 1000 + j * 10, 3).await;
                }
            }));
        }
    }
    for j in joins {
        j.await.unwrap();
    }
    repo.sync_index().await.unwrap();
    let victims: Vec<Entry<'static>> = live_entries(&a).await[1..4].to_vec();
    let (ida, idb) = (a.id(), b.id());
    let resolver = Arc::clone(&a.core.resolver);
    let opts = Limits::new(repo.config()).collection_opts;
    repo.shutdown().await.unwrap();
    drop((repo, a, b));
    let mut c = Collection::open(
        &coll_dir(&td.repo(), ida),
        &opts,
        &RecoveryOptions::default(),
        &*resolver,
    )
    .unwrap()
    .0;
    c.tombstone(&victims).unwrap();
    drop(c);
    let (repo, _) = Repository::open(td.repo()).await.unwrap();
    let (a, b) = (repo.collection_by_id(ida).unwrap(), repo.collection_by_id(idb).unwrap());
    put_n(&a, 50_000, 10).await;
    put_n(&b, 60_000, 10).await;
    repo.sync_index().await.unwrap();
    let (inc_a, inc_b) = (index_dump(&repo, ida).await, index_dump(&repo, idb).await);
    assert_eq!(inc_a, live_entries(&a).await);
    assert_eq!(inc_b, live_entries(&b).await);
    assert!(victims.iter().all(|v| !inc_a.contains(v)));
    repo.rebuild_index().await.unwrap();
    assert_eq!(index_dump(&repo, ida).await, inc_a);
    assert_eq!(index_dump(&repo, idb).await, inc_b);
    repo.shutdown().await.unwrap();
    let (repo, _) = Repository::open(td.repo()).await.unwrap();
    repo.sync_index().await.unwrap();
    assert_eq!(index_dump(&repo, ida).await, inc_a);
    assert_eq!(index_dump(&repo, idb).await, inc_b);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rebuild_index_concurrent_puts_lose_nothing() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &small_config()).await.unwrap();
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    let rebuild = {
        let repo = repo.clone();
        tokio::spawn(async move { repo.rebuild_index().await })
    };
    let mut joins = Vec::new();
    for t in 0..4 {
        let h = h.clone();
        joins.push(tokio::spawn(async move {
            let mut out = Vec::new();
            for j in 0..50 {
                out.extend(h.put(vec![record(t * 1000 + j, 40)]).await.unwrap());
            }
            out
        }));
    }
    let mut located = Vec::new();
    for j in joins {
        located.extend(j.await.unwrap());
    }
    rebuild.await.unwrap().unwrap();
    repo.sync_index().await.unwrap();
    let dump = index_dump(&repo, h.id()).await;
    assert_eq!(dump.len(), 201);
    assert_i8(&repo, &h).await;
    let got: BTreeSet<_> = dump.iter().map(|e| (e.segment, e.offset)).collect();
    for l in &located {
        assert!(got.contains(&(l.entry.segment, l.entry.offset)));
    }
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rebuild_index_is_exclusive() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    put_n(&h, 0, 30).await;
    let (r1, r2) = tokio::join!(repo.rebuild_index(), repo.rebuild_index());
    r1.unwrap();
    r2.unwrap();
    repo.sync_index().await.unwrap();
    assert_eq!(index_dump(&repo, h.id()).await.len(), 31);
    assert_i8(&repo, &h).await;
    repo.shutdown().await.unwrap();
}
}

#[cfg(feature = "redb")]
rt_test! {
async fn orphan_index_collection_dropped_skipped_kept() {
    let td = TestDir::new();
    let repo = repo_with_dict(&td, &Config::default()).await;
    let s = repo.create_collection("s", zstd_codec()).await.unwrap();
    let a = repo.create_collection("a", Codec::None).await.unwrap();
    put_n(&s, 0, 3).await;
    repo.sync_index().await.unwrap();
    let e = live_entries(&s).await[1].clone();
    let (sid, aid) = (s.id(), a.id());
    repo.shutdown().await.unwrap();
    drop((repo, s, a));
    let orphan = CollectionKey::new(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
    {
        let ix = direct::open(&td.repo());
        ix.apply(&[
            Op::ReplaceGeneration { collection: orphan, generation: 1 },
            Op::Insert { collection: orphan, generation: 1, entry: e },
        ])
        .unwrap();
        assert!(ix.collections().unwrap().contains(&orphan));
    }
    fs::remove_file(td.repo().join("data/dict/zstd").join(format!("test.{}.zstdict", dict_id()))).unwrap();
    let (repo, report) = Repository::open(td.repo()).await.unwrap();
    assert_eq!(report.skipped.len(), 1);
    repo.sync_index().await.unwrap();
    let cols = repo.core().index().unwrap().collections().unwrap();
    assert!(!cols.contains(&orphan));
    assert!(cols.contains(&CollectionKey::new(sid.as_u128())));
    assert!(cols.contains(&CollectionKey::new(aid.as_u128())));
    repo.shutdown().await.unwrap();
}
}

#[cfg(feature = "redb")]
rt_test! {
async fn stale_staged_generation_discarded_at_open() {
    let td = TestDir::new();
    let (id, _, e) = shut_one(&td, 5).await;
    let k = CollectionKey::new(id.as_u128());
    {
        let ix = direct::open(&td.repo());
        ix.apply(&[
            Op::Insert { collection: k, generation: 2, entry: e },
            Op::Checkpoint { collection: k, checkpoint: Checkpoint { generation: 2, cdx_offset: 10 } },
        ])
        .unwrap();
        assert!(ix.state(k).unwrap().unwrap().staged_checkpoint.is_some());
    }
    let (repo, _) = Repository::open(td.repo()).await.unwrap();
    repo.sync_index().await.unwrap();
    let h = repo.collection_by_id(id).unwrap();
    assert_eq!(index_state(&repo, &h).staged_checkpoint, None);
    assert_i8(&repo, &h).await;
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn apply_failure_recovers() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    repo.sync_index().await.unwrap();
    let hooks = index_hooks(&repo);
    hooks.fail_applies.store(1, Ordering::Release);
    put_n(&h, 0, 5).await;
    repo.sync_index().await.unwrap(); // the barrier retries
    assert_i8(&repo, &h).await;
    hooks.fail_applies.store(2, Ordering::Release);
    put_n(&h, 100, 5).await;
    match repo.sync_index().await {
        Err(Error::IndexLagging { collections }) => assert!(collections.contains(&h.id())),
        other => panic!("{other:?}"),
    }
    repo.sync_index().await.unwrap();
    assert_eq!(index_dump(&repo, h.id()).await.len(), 11);
    assert_i8(&repo, &h).await;
    // The track failed before but succeeded since: a rebuild gets its retry again.
    hooks.fail_applies.store(1, Ordering::Release);
    repo.rebuild_index().await.unwrap();
    assert_i8(&repo, &h).await;
    repo.shutdown().await.unwrap();
}
}

#[cfg(feature = "redb")]
rt_test! {
async fn shutdown_drains_index() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    repo.sync_index().await.unwrap();
    let sem = arm(&repo);
    put_n(&h, 0, 25).await;
    let (id, cdx_len) = (h.id(), h.stats().cdx_len);
    let sd = {
        let repo = repo.clone();
        tokio::spawn(async move { repo.shutdown().await })
    };
    disarm(&repo);
    sem.add_permits(1000);
    sd.await.unwrap().unwrap();
    drop((repo, h));
    let ix = direct::open(&td.repo());
    assert_eq!(dump_index(&ix, id).len(), 26);
    let cp = ix.state(CollectionKey::new(id.as_u128())).unwrap().unwrap().checkpoint.unwrap();
    assert_eq!(cp.cdx_offset, cdx_len);
}
}

rt_test! {
async fn purge_loop_runs_and_stops() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    put_n(&h, 0, 50).await;
    repo.sync_index().await.unwrap();
    let hooks = index_hooks(&repo);
    let idle = hooks.purge_idle.subscribe();
    let mut parked = hooks.parked.subscribe();
    repo.rebuild_index().await.unwrap();
    // Parked = every purge step is done (the task purges before it waits).
    parked.wait_for(|p| *p).await.unwrap();
    assert_eq!(repo.core().index().unwrap().stored_entries().unwrap(), 51);
    let v = *idle.borrow();
    assert!(v >= 1);
    repo.sync_index().await.unwrap();
    parked.wait_for(|p| *p).await.unwrap();
    assert_eq!(*idle.borrow(), v, "purge must not run again without structural work");
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn memory_backend_reindexes_at_every_open() {
    let td = TestDir::new();
    use_memory_index(&td.repo());
    let repo = Repository::init(td.repo()).await.unwrap();
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    put_n(&h, 0, 10).await;
    repo.sync_index().await.unwrap();
    assert_i8(&repo, &h).await;
    let id = h.id();
    repo.shutdown().await.unwrap();
    drop((repo, h));
    let (repo, _) = Repository::open(td.repo()).await.unwrap();
    repo.sync_index().await.unwrap();
    let h = repo.collection_by_id(id).unwrap();
    assert_eq!(index_dump(&repo, id).await.len(), 11);
    assert_i8(&repo, &h).await;
    assert!(index_hooks(&repo).reindexes.load(Ordering::Acquire) >= 1);
    repo.shutdown().await.unwrap();
}
}

#[cfg(feature = "redb")]
rt_test! {
async fn drop_without_shutdown_drains_index() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    let h = repo.create_collection("a", Codec::None).await.unwrap();
    put_n(&h, 0, 10).await;
    let id = h.id();
    drop(h);
    drop(repo);
    let mut opened = None;
    for _ in 0..5000 {
        match Repository::open(td.repo()).await {
            Ok((r, _)) => {
                opened = Some(r);
                break;
            }
            Err(Error::Locked { .. } | Error::IndexOpen { .. }) => {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            Err(e) => panic!("{e:?}"),
        }
    }
    let repo = opened.expect("lock never released");
    repo.sync_index().await.unwrap();
    let h = repo.collection_by_id(id).unwrap();
    assert_eq!(index_dump(&repo, id).await.len(), 11);
    assert_i8(&repo, &h).await;
    assert_eq!(index_hooks(&repo).reindexes.load(Ordering::Acquire), 0);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn index_calls_after_shutdown() {
    let td = TestDir::new();
    let repo = Repository::init(td.repo()).await.unwrap();
    repo.shutdown().await.unwrap();
    assert!(matches!(repo.sync_index().await, Err(Error::ShuttingDown)));
    assert!(matches!(repo.rebuild_index().await, Err(Error::ShuttingDown)));
}
}

/// Measures `index_sync_every` 1 / 16 / 64:
/// `cargo test --release -p masstuffy bench_index_sync_every -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark: run explicitly in release mode"]
async fn bench_index_sync_every() {
    use std::time::Instant;
    for n in [1u32, 16, 64] {
        let td = TestDir::new();
        let cfg = Config {
            index_sync_every: n,
            ..small_config()
        };
        let repo = Repository::init_with_config(td.repo(), &cfg).await.unwrap();
        let h = repo.create_collection("a", Codec::None).await.unwrap();
        repo.sync_index().await.unwrap();
        let t0 = Instant::now();
        let mut joins = Vec::new();
        for t in 0..8usize {
            let h = h.clone();
            joins.push(tokio::spawn(async move {
                for j in 0..250usize {
                    h.put(vec![record(t * 1000 + j, 2048)]).await.unwrap();
                }
            }));
        }
        for j in joins {
            j.await.unwrap();
        }
        let acked = t0.elapsed();
        let t1 = Instant::now();
        repo.sync_index().await.unwrap();
        let sync = t1.elapsed();
        println!(
            "index_sync_every={n}: 2000 puts acked in {acked:?}, sync_index after last ack {sync:?}, total {:?}",
            t0.elapsed()
        );
        repo.shutdown().await.unwrap();
    }
}
