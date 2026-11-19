//! M24 tests: the shared circular grep scan.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use masstuffy_cdx::Entry;
use masstuffy_collection::Codec;
use masstuffy_warc::{Record, RecordBuilder, WarcDate};
use tokio::sync::Semaphore;

use crate::collection::CollectionHandle;
use crate::error::Error;
use crate::grep::{
    Grep, GrepEvent, GrepEvents, GrepOptions, GrepQuery, GrepReport, GrepScope,
    MAX_CONCURRENT_GREPS, MAX_GREP_COLLECTIONS, MAX_GREP_HITS, MAX_GREP_PATTERN_BYTES,
    MAX_GREP_QUERIES, QuerySet,
};
use crate::grep_scan::GrepHooks;
use crate::repository::Repository;
use crate::testutil::*;
use crate::types::Located;

macro_rules! rt_test {
    ($(#[$m:meta])* async fn $name:ident() $body:block) => {
        $(#[$m])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            bounded(async $body).await
        }
    };
}

/// A `resource` record with an explicit id and the given body.
fn rec(i: usize, body: &str) -> Record {
    let mut b = RecordBuilder::new("resource").unwrap();
    b.record_id(&id_str(i)).unwrap();
    b.date(WarcDate::parse(DATE).unwrap());
    b.target_uri(&url(i)).unwrap();
    b.build(body.as_bytes()).unwrap()
}

fn body(i: usize, needle: bool) -> String {
    if needle {
        format!("filler {i} needle filler")
    } else {
        format!("filler {i} plain filler")
    }
}

fn gq(p: &str) -> GrepQuery {
    GrepQuery {
        pattern: p.to_owned(),
        regex: false,
        case_insensitive: false,
    }
}

fn opts(patterns: &[&str]) -> GrepOptions {
    GrepOptions::new(patterns.iter().map(|p| gq(p)).collect())
}

/// One query `needle`, hit events only (message counts then equal hit counts).
fn hits_only() -> GrepOptions {
    let mut o = opts(&["needle"]);
    o.events = GrepEvents {
        hits: true,
        ..GrepEvents::NONE
    };
    o
}

fn hooks(h: &CollectionHandle) -> Arc<GrepHooks> {
    Arc::clone(&h.shared.grep.hooks)
}

/// Installs a closed gate (no permit) and a batch size on `h`'s scanner.
fn gate(h: &CollectionHandle, batch_lines: usize) -> Arc<Semaphore> {
    let sem = Arc::new(Semaphore::new(0));
    let hk = hooks(h);
    hk.batch_lines.store(batch_lines, Ordering::Release);
    *hk.gate.lock().unwrap() = Some(Arc::clone(&sem));
    sem
}

fn open_gate(sem: &Semaphore) {
    sem.add_permits(Semaphore::MAX_PERMITS / 2);
}

async fn batches_reach(h: &CollectionHandle, n: u64) {
    let mut rx = hooks(h).batches.subscribe();
    rx.wait_for(|b| *b >= n).await.unwrap();
}

async fn exited_reach(h: &CollectionHandle, n: u64) {
    let mut rx = hooks(h).exited.subscribe();
    rx.wait_for(|b| *b >= n).await.unwrap();
}

struct Drained {
    hits: Vec<(Arc<Located>, QuerySet)>,
    unreadable: Vec<(Arc<Located>, Arc<Error>)>,
    /// `(cdx_done, cdx_total)` of each `Progress` event, in order.
    progress: Vec<(u64, u64)>,
    report: GrepReport,
}

impl Drained {
    fn ids(&self) -> BTreeSet<String> {
        self.hits
            .iter()
            .map(|(l, _)| l.entry.record_id.to_string())
            .collect()
    }

    /// The progress events start at 0, strictly increase and stay within the total.
    fn assert_progress(&self, total: u64) {
        assert_eq!(self.progress.first().map(|p| p.0), Some(0));
        assert!(self.progress.iter().all(|p| p.1 == total));
        assert!(self.progress.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(self.progress.last().is_some_and(|p| p.0 <= total));
    }

    /// Every hit appears once.
    fn assert_unique(&self) {
        assert_eq!(self.ids().len(), self.hits.len(), "duplicate hit");
    }
}

async fn drain(g: &mut Grep) -> Drained {
    let mut d = Drained {
        hits: Vec::new(),
        unreadable: Vec::new(),
        progress: Vec::new(),
        report: GrepReport {
            collections: Vec::new(),
            hits: 0,
            truncated: false,
        },
    };
    while let Some(ev) = g.next().await {
        match ev {
            GrepEvent::Hit { located, queries } => d.hits.push((located, queries)),
            GrepEvent::Unreadable { located, error } => d.unreadable.push((located, error)),
            GrepEvent::Progress {
                cdx_done,
                cdx_total,
                ..
            } => d.progress.push((cdx_done, cdx_total)),
        }
    }
    d.report = g.report().unwrap();
    assert!(g.report().is_none());
    d
}

fn ids_where(range: std::ops::Range<usize>, f: impl Fn(usize) -> bool) -> BTreeSet<String> {
    range.filter(|&i| f(i)).map(id_str).collect()
}

/// A repository with one `Codec::None` collection `c` holding `n` records `0..n` (one put), the
/// ones for which `needle` is true carrying the needle.
async fn setup(
    n: usize,
    needle: impl Fn(usize) -> bool,
) -> (TestDir, Repository, CollectionHandle, Vec<Located>) {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &small_config())
        .await
        .unwrap();
    let h = repo.create_collection("c", Codec::None).await.unwrap();
    let located = put_range(&h, 0..n, &needle).await;
    (td, repo, h, located)
}

async fn put_range(
    h: &CollectionHandle,
    range: std::ops::Range<usize>,
    needle: &impl Fn(usize) -> bool,
) -> Vec<Located> {
    let recs = range.map(|i| rec(i, &body(i, needle(i)))).collect();
    h.put(recs).await.unwrap()
}

/// Length of the CDX header line (with its newline) of the active generation of `h`.
fn header_len(repo: &Repository, h: &CollectionHandle) -> u64 {
    let path = repo
        .path()
        .join("data/repository")
        .join(h.id().to_string())
        .join(format!("index.{}.cdx", h.info().generation));
    let bytes = std::fs::read(path).unwrap();
    let n = bytes.iter().position(|&b| b == b'\n').unwrap();
    u64::try_from(n + 1).unwrap()
}

fn key(e: &Entry<'_>) -> (u32, u64) {
    (e.segment, e.offset)
}

rt_test! {
async fn grep_finds_all_matches() {
    let (_td, repo, h, _) = setup(50, |i| i % 2 == 1).await;
    let total = h.stats().cdx_len - header_len(&repo, &h);
    let mut g = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap();
    let d = drain(&mut g).await;
    d.assert_unique();
    assert_eq!(d.ids(), ids_where(0..50, |i| i % 2 == 1));
    let c = &d.report.collections[0];
    assert!(c.stats.complete && c.error.is_none());
    assert_eq!((c.stats.records, c.stats.hits), (51, 25));
    assert_eq!((c.stats.cdx_done, c.stats.cdx_total), (total, total));
    assert_eq!(d.progress.first(), Some(&(0, total)));
    assert_eq!(d.report.hits, 25);
    assert!(!d.report.truncated);
    assert!(d.hits.iter().all(|(_, q)| q.bits() == 1));
    assert_eq!(hooks(&h).started.load(Ordering::Acquire), 1);
    exited_reach(&h, 1).await;
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn grep_scopes() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &small_config()).await.unwrap();
    let h = repo.create_collection("c", Codec::None).await.unwrap();
    let http = |i: usize, b: &str| {
        let mut r = RecordBuilder::new("response").unwrap();
        r.record_id(&id_str(i)).unwrap();
        r.date(WarcDate::parse(DATE).unwrap());
        r.header("Content-Type", "application/http; msgtype=response").unwrap();
        r.build(b.as_bytes()).unwrap()
    };
    let mut d = RecordBuilder::new("resource").unwrap();
    d.record_id(&id_str(4)).unwrap();
    d.date(WarcDate::parse(DATE).unwrap());
    d.target_uri("https://example.org/needle").unwrap();
    h.put(vec![
        rec(1, "xx needle xx"),
        http(2, "HTTP/1.1 200 OK\r\nX-Note: needle\r\n\r\nplain"),
        http(3, "HTTP/1.1 200 OK\r\n\r\nneedle here"),
        d.build(b"plain").unwrap(),
    ])
    .await
    .unwrap();
    let set = |v: &[usize]| v.iter().map(|&i| id_str(i)).collect::<BTreeSet<_>>();
    for (scope, want) in [
        (GrepScope::Payload, set(&[1, 3])),
        (GrepScope::Headers, set(&[2, 4])),
        (GrepScope::All, set(&[1, 2, 3, 4])),
    ] {
        let mut o = opts(&["needle"]);
        o.scope = scope;
        let mut g = repo.grep(std::slice::from_ref(&h), o).await.unwrap();
        let d = drain(&mut g).await;
        assert_eq!(d.ids(), want, "{scope:?}");
    }
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn grep_multi_query_reports_set() {
    let td = TestDir::new();
    let repo = Repository::init_with_config(td.repo(), &small_config()).await.unwrap();
    let h = repo.create_collection("c", Codec::None).await.unwrap();
    h.put(vec![
        rec(1, "only alpha"),
        rec(2, "only beta"),
        rec(3, "alpha and beta"),
        rec(4, "nothing"),
    ])
    .await
    .unwrap();
    let mut g = repo.grep(std::slice::from_ref(&h), opts(&["alpha", "beta"])).await.unwrap();
    let d = drain(&mut g).await;
    let by: BTreeMap<String, u64> = d
        .hits
        .iter()
        .map(|(l, q)| (l.entry.record_id.to_string(), q.bits()))
        .collect();
    assert_eq!(
        by,
        BTreeMap::from([(id_str(1), 0b01), (id_str(2), 0b10), (id_str(3), 0b11)])
    );
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn second_grep_joins_mid_scan() {
    let (_td, repo, h, _) = setup(60, |i| i % 3 == 0).await;
    let sem = gate(&h, 4);
    hooks(&h).progress_lines.store(3, Ordering::Release);
    let mut a = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap();
    sem.add_permits(3);
    batches_reach(&h, 3).await;
    let mut b = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap();
    open_gate(&sem);
    let ta = tokio::spawn(async move { drain(&mut a).await });
    let tb = tokio::spawn(async move { drain(&mut b).await });
    let (da, db) = (ta.await.unwrap(), tb.await.unwrap());
    let want = ids_where(0..60, |i| i % 3 == 0);
    let total = da.report.collections[0].stats.cdx_total;
    assert_eq!(db.report.collections[0].stats.cdx_total, total);
    for d in [&da, &db] {
        d.assert_progress(total);
        assert_eq!(d.report.collections[0].stats.cdx_done, total);
        d.assert_unique();
        assert_eq!(d.ids(), want);
        let c = &d.report.collections[0];
        assert!(c.stats.complete && c.error.is_none());
        assert_eq!(c.stats.records, 61);
    }
    let hk = hooks(&h);
    assert_eq!(hk.started.load(Ordering::Acquire), 1);
    assert_eq!(hk.passes.load(Ordering::Acquire), 1);

    // Every record read once, except the CDX prefix before B's start, read twice.
    let live = live_entries(&h).await;
    assert_eq!(live.len(), 61);
    let reads = hk.reads.lock().unwrap().clone();
    for (n, e) in live.iter().enumerate() {
        let want = if n < 12 { 2 } else { 1 };
        assert_eq!(reads.get(&key(e)).copied(), Some(want), "entry {n}");
    }

    // B's delivery order: from its start to the end, then one wrap.
    let pos: BTreeMap<String, usize> = live
        .iter()
        .enumerate()
        .map(|(n, e)| (e.record_id.to_string(), n))
        .collect();
    let order: Vec<usize> = db.hits.iter().map(|(l, _)| pos[&*l.entry.record_id]).collect();
    assert!(order.first().is_some_and(|&n| n >= 12));
    assert!(order.windows(2).filter(|w| w[0] > w[1]).count() <= 1);
    assert!(order.windows(2).any(|w| w[0] > w[1]), "B must have wrapped");
    exited_reach(&h, 1).await;
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn many_joiners_concurrently() {
    let (_td, repo, h, _) = setup(60, |i| i % 3 == 0).await;
    let sem = gate(&h, 4);
    let mut greps = vec![repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap()];
    sem.add_permits(1);
    for k in 1..8u64 {
        batches_reach(&h, k).await;
        greps.push(repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap());
        sem.add_permits(1);
    }
    open_gate(&sem);
    let tasks: Vec<_> = greps
        .into_iter()
        .map(|mut g| tokio::spawn(async move { drain(&mut g).await }))
        .collect();
    let want = ids_where(0..60, |i| i % 3 == 0);
    for t in tasks {
        let d = t.await.unwrap();
        d.assert_unique();
        assert_eq!(d.ids(), want);
        assert!(d.report.collections[0].stats.complete);
    }
    let hk = hooks(&h);
    assert_eq!(hk.started.load(Ordering::Acquire), 1);
    let reads = hk.reads.lock().unwrap().clone();
    assert!(reads.values().all(|&n| (1..=2).contains(&n)));
    assert!(reads.values().map(|&n| u64::from(n)).sum::<u64>() <= 2 * 61);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn grep_excludes_deleted() {
    let (_td, repo, h, located) = setup(20, |_| true).await;
    let gone: Vec<Located> = located[..5].to_vec();
    assert_eq!(h.delete(gone).await.unwrap(), 5);
    let mut g = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap();
    let d = drain(&mut g).await;
    assert_eq!(d.ids(), ids_where(5..20, |_| true));
    assert_eq!(d.report.collections[0].stats.records, 16);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn deleted_during_scan_not_retracted_but_later_hidden() {
    let (_td, repo, h, located) = setup(20, |_| true).await;
    let sem = gate(&h, 4);
    let mut a = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap();
    sem.add_permits(2);
    batches_reach(&h, 2).await;
    assert_eq!(h.delete(vec![located[18].clone()]).await.unwrap(), 1);
    let mut b = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap();
    open_gate(&sem);
    let ta = tokio::spawn(async move { drain(&mut a).await });
    let tb = tokio::spawn(async move { drain(&mut b).await });
    let (da, db) = (ta.await.unwrap(), tb.await.unwrap());
    assert_eq!(db.ids(), ids_where(0..20, |i| i != 18));
    da.assert_unique();
    assert!(da.report.collections[0].stats.complete);
    assert!(da.ids().is_superset(&ids_where(0..20, |i| i != 18)));
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn growth_during_scan() {
    let (_td, repo, h, _) = setup(20, |_| true).await;
    let sem = gate(&h, 4);
    let t_a = h.stats().cdx_len;
    let mut a = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap();
    put_range(&h, 20..30, &|_| true).await;
    let t_b = h.stats().cdx_len;
    let mut b = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap();
    open_gate(&sem);
    let ta = tokio::spawn(async move { drain(&mut a).await });
    let tb = tokio::spawn(async move { drain(&mut b).await });
    let (da, db) = (ta.await.unwrap(), tb.await.unwrap());
    assert_eq!(da.ids(), ids_where(0..20, |_| true));
    assert_eq!(db.ids(), ids_where(0..30, |_| true));
    let (sa, sb) = (da.report.collections[0].stats, db.report.collections[0].stats);
    assert_eq!(sb.cdx_total - sa.cdx_total, t_b - t_a);
    assert_eq!((sa.cdx_done, sb.cdx_done), (sa.cdx_total, sb.cdx_total));
    assert!(da.report.collections[0].stats.complete && db.report.collections[0].stats.complete);
    let hk = hooks(&h);
    assert_eq!(hk.started.load(Ordering::Acquire), 1);
    let reads = hk.reads.lock().unwrap().clone();
    let live = live_entries(&h).await;
    for e in &live[21..] {
        assert_eq!(reads.get(&key(e)).copied(), Some(1));
    }
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn drop_grep_detaches_and_scanner_stops() {
    let (_td, repo, h, _) = setup(60, |i| i % 3 == 0).await;
    let sem = gate(&h, 4);
    let mut a = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap();
    let b = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap();
    sem.add_permits(2);
    batches_reach(&h, 2).await;
    drop(b);
    open_gate(&sem);
    let d = drain(&mut a).await;
    assert_eq!(d.ids(), ids_where(0..60, |i| i % 3 == 0));
    assert!(d.report.collections[0].stats.complete);
    exited_reach(&h, 1).await;

    // A second run: drop the only grep mid-scan.
    let sem = gate(&h, 4);
    let base = *hooks(&h).batches.borrow();
    let c = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap();
    sem.add_permits(2);
    batches_reach(&h, base + 2).await;
    drop(c);
    open_gate(&sem);
    exited_reach(&h, 2).await;
    // The scanner stopped: no further batch runs (a batch counter bump needs a live scanner).
    let after = *hooks(&h).batches.borrow();
    assert!(after <= base + 2, "{after} > {}", base + 2);
    assert_eq!(hooks(&h).started.load(Ordering::Acquire), 2);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn lagged_consumer_is_detached() {
    let (_td, repo, h, _) = setup(200, |_| true).await;
    let sem = gate(&h, 1);
    let mut a = repo.grep(std::slice::from_ref(&h), hits_only()).await.unwrap();
    let mut b = repo.grep(std::slice::from_ref(&h), hits_only()).await.unwrap();
    // The warcinfo line: no hit, no message.
    sem.add_permits(1);
    batches_reach(&h, 1).await;
    // B keeps up (it is read in step with the scan); A is never polled.
    let mut b_hits = Vec::new();
    let mut done = 1u64;
    for _ in 0..5 {
        sem.add_permits(32);
        done += 32;
        batches_reach(&h, done).await;
        for _ in 0..32 {
            match b.next().await {
                Some(GrepEvent::Hit { located, .. }) => b_hits.push(located),
                other => panic!("unexpected {other:?}"),
            }
        }
    }
    open_gate(&sem);
    let rest = drain(&mut b).await;
    let mut all: BTreeSet<String> = b_hits.iter().map(|l| l.entry.record_id.to_string()).collect();
    all.extend(rest.ids());
    assert_eq!(all, ids_where(0..200, |_| true));
    assert!(rest.report.collections[0].stats.complete);

    // A got at most 64 batches, then the end, and was detached as lagging.
    let mut a_events = 0;
    while let Some(ev) = a.next().await {
        assert!(matches!(ev, GrepEvent::Hit { .. }));
        a_events += 1;
    }
    assert_eq!(a_events, 64);
    let rep = a.report().unwrap();
    let c = &rep.collections[0];
    assert!(!c.stats.complete);
    assert!(matches!(c.error.as_deref(), Some(Error::GrepLagged)), "{:?}", c.error);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn max_hits_truncates() {
    let (_td, repo, h, _) = setup(30, |_| true).await;
    let mut o = opts(&["needle"]);
    o.max_hits = 7;
    let mut g = repo.grep(std::slice::from_ref(&h), o).await.unwrap();
    let d = drain(&mut g).await;
    assert_eq!(d.hits.len(), 7);
    assert_eq!(d.report.hits, 7);
    assert!(d.report.truncated);
    let c = &d.report.collections[0];
    assert!(!c.stats.complete && c.error.is_none());
    assert_eq!(c.stats.hits, 7);
    assert!(c.stats.cdx_done < c.stats.cdx_total);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn multi_collection_grep() {
    let (_td, repo, h1, _) = setup(10, |i| i % 2 == 0).await;
    let h2 = repo.create_collection("d", Codec::None).await.unwrap();
    put_range(&h2, 100..110, &|i| i % 5 == 0).await;
    let mut g = repo.grep(&[h2.clone(), h1.clone()], opts(&["needle"])).await.unwrap();
    let d = drain(&mut g).await;
    let of = |h: &CollectionHandle| -> BTreeSet<String> {
        d.hits
            .iter()
            .filter(|(l, _)| l.collection == h.id())
            .map(|(l, _)| l.entry.record_id.to_string())
            .collect()
    };
    assert_eq!(of(&h1), ids_where(0..10, |i| i % 2 == 0));
    assert_eq!(of(&h2), ids_where(100..110, |i| i % 5 == 0));
    let order: Vec<_> = d.report.collections.iter().map(|c| c.collection).collect();
    assert_eq!(order, vec![h2.id(), h1.id()]);
    assert!(d.report.collections.iter().all(|c| c.stats.complete));
    assert_eq!(hooks(&h1).started.load(Ordering::Acquire), 1);
    assert_eq!(hooks(&h2).started.load(Ordering::Acquire), 1);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn rebuild_mid_scan_fails_stale() {
    let td = TestDir::new();
    let repo = repo_with_dict(&td, &small_config()).await;
    let h = repo.create_collection("c", Codec::None).await.unwrap();
    put_range(&h, 0..40, &|_| true).await;
    let sem = gate(&h, 4);
    let mut g = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap();
    sem.add_permits(1);
    batches_reach(&h, 1).await;
    h.rebuild(zstd_codec(), crate::rebuild::RebuildOptions::default()).await.unwrap();
    open_gate(&sem);
    let d = drain(&mut g).await;
    let c = &d.report.collections[0];
    assert!(!c.stats.complete);
    assert!(matches!(c.error.as_deref(), Some(Error::StaleLocator { .. })), "{:?}", c.error);
    assert!(c.stats.cdx_total > 0 && c.stats.cdx_done < c.stats.cdx_total);
    exited_reach(&h, 1).await;
    // A new grep completes on the new generation.
    let mut g = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap();
    let d = drain(&mut g).await;
    assert_eq!(d.ids(), ids_where(0..40, |_| true));
    assert!(d.report.collections[0].stats.complete);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn shutdown_ends_grep() {
    let (_td, repo, h, _) = setup(20, |_| true).await;
    let _sem = gate(&h, 4);
    let mut g = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap();
    repo.shutdown().await.unwrap();
    let d = drain(&mut g).await;
    let c = &d.report.collections[0];
    assert!(!c.stats.complete);
    assert!(matches!(c.error.as_deref(), Some(Error::ShuttingDown)), "{:?}", c.error);
    assert!(c.stats.cdx_done < c.stats.cdx_total || c.stats.cdx_total == 0);
    let e = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.err().unwrap();
    assert!(matches!(e, Error::ShuttingDown), "{e:?}");
}
}

rt_test! {
async fn delete_collection_ends_grep() {
    let (_td, repo, h, _) = setup(20, |_| true).await;
    let sem = gate(&h, 4);
    let mut g = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap();
    repo.delete_collection("c").await.unwrap();
    open_gate(&sem);
    let d = drain(&mut g).await;
    let c = &d.report.collections[0];
    assert!(!c.stats.complete);
    assert!(matches!(c.error.as_deref(), Some(Error::UnknownCollection(_))), "{:?}", c.error);
    assert!(c.stats.cdx_done < c.stats.cdx_total || c.stats.cdx_total == 0);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn too_many_greps() {
    let (_td, repo, h, _) = setup(10, |_| true).await;
    let _sem = gate(&h, 4);
    let mut held = Vec::new();
    for _ in 0..MAX_CONCURRENT_GREPS {
        held.push(repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap());
    }
    let e = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.err().unwrap();
    assert!(matches!(e, Error::TooManyGreps), "{e:?}");
    held.pop();
    let ok = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await;
    assert!(ok.is_ok());
    drop(held);
    drop(ok);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn validation_errors() {
    let (_td, repo, h, _) = setup(3, |_| true).await;
    let h2 = repo.create_collection("d", Codec::None).await.unwrap();
    let other_td = TestDir::new();
    let other = Repository::init_with_config(other_td.repo(), &small_config()).await.unwrap();
    let oh = other.create_collection("c", Codec::None).await.unwrap();

    async fn err(repo: &Repository, hs: &[CollectionHandle], o: GrepOptions) -> Error {
        repo.grep(hs, o).await.err().unwrap()
    }
    let q = |e: &Error, want: &str| {
        assert!(matches!(e, Error::InvalidQuery(w) if *w == want), "{e:?}");
    };
    let ok = || opts(&["needle"]);
    q(&err(&repo, &[], ok()).await, "grep collections");
    let many: Vec<CollectionHandle> = (0..=MAX_GREP_COLLECTIONS).map(|_| h.clone()).collect();
    q(&err(&repo, &many, ok()).await, "grep collections");
    q(&err(&repo, &[h.clone(), h2.clone(), h.clone()], ok()).await, "grep collections");
    q(&err(&repo, std::slice::from_ref(&oh), ok()).await, "grep collections");
    q(&err(&repo, std::slice::from_ref(&h), GrepOptions::new(vec![])).await, "grep queries");
    let too_many = GrepOptions::new((0..=MAX_GREP_QUERIES).map(|i| gq(&format!("q{i}"))).collect());
    q(&err(&repo, std::slice::from_ref(&h), too_many).await, "grep queries");
    q(&err(&repo, std::slice::from_ref(&h), opts(&[""])).await, "grep pattern");
    let long = "a".repeat(MAX_GREP_PATTERN_BYTES + 1);
    q(&err(&repo, std::slice::from_ref(&h), opts(&[&long])).await, "grep pattern");
    let mut o = ok();
    o.max_hits = 0;
    q(&err(&repo, std::slice::from_ref(&h), o).await, "grep max_hits");
    let mut o = ok();
    o.max_hits = MAX_GREP_HITS + 1;
    q(&err(&repo, std::slice::from_ref(&h), o).await, "grep max_hits");
    let bad = GrepOptions::new(vec![gq("ok"), GrepQuery { pattern: "(".into(), regex: true, case_insensitive: false }]);
    let e = err(&repo, std::slice::from_ref(&h), bad).await;
    assert!(matches!(e, Error::InvalidPattern { index: Some(1), .. }), "{e:?}");
    // The longest allowed pattern and the most queries are accepted.
    let edge = "a".repeat(MAX_GREP_PATTERN_BYTES);
    assert!(repo.grep(std::slice::from_ref(&h), opts(&[&edge])).await.is_ok());
    // A deleted collection.
    repo.delete_collection("d").await.unwrap();
    let e = err(&repo, std::slice::from_ref(&h2), ok()).await;
    assert!(matches!(e, Error::UnknownCollection(_)), "{e:?}");
    other.shutdown().await.unwrap();
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn unreadable_record_reported() {
    let (_td, repo, h, located) = setup(30, |i| i % 2 == 1).await;
    let target = located[5].clone();
    let dir = repo.path().join("data/repository").join(h.id().to_string());
    let seg = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(last_segment(&dir))
        .unwrap();
    let at = target.entry.offset + target.entry.raw_size / 2;
    let mut b = [0u8; 1];
    seg.read_exact_at(&mut b, at).unwrap();
    b[0] ^= 0xFF;
    seg.write_all_at(&b, at).unwrap();
    drop(seg);

    let mut g = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap();
    let d = drain(&mut g).await;
    assert_eq!(d.unreadable.len(), 1);
    assert_eq!(key(&d.unreadable[0].0.entry), key(&target.entry));
    assert_eq!(d.ids(), ids_where(0..30, |i| i % 2 == 1 && i != 5));
    let c = &d.report.collections[0];
    assert_eq!((c.stats.unreadable, c.stats.records, c.stats.hits), (1, 31, 14));
    assert!(c.stats.complete);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn zstd_collection_grep() {
    let td = TestDir::new();
    let repo = repo_with_dict(&td, &small_config()).await;
    let h = repo.create_collection("c", zstd_codec()).await.unwrap();
    put_range(&h, 0..50, &|i| i % 2 == 1).await;
    let mut g = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap();
    let d = drain(&mut g).await;
    d.assert_unique();
    assert_eq!(d.ids(), ids_where(0..50, |i| i % 2 == 1));
    let c = &d.report.collections[0];
    assert!(c.stats.complete);
    assert_eq!((c.stats.records, c.stats.hits), (51, 25));
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn events_none_counts_only() {
    let (_td, repo, h, _) = setup(30, |i| i % 3 == 0).await;
    hooks(&h).progress_lines.store(2, Ordering::Release);
    let mut o = opts(&["needle"]);
    o.events = GrepEvents::NONE;
    let mut g = repo.grep(std::slice::from_ref(&h), o.clone()).await.unwrap();
    assert!(g.next().await.is_none());
    let rep = g.report().unwrap();
    let c = &rep.collections[0];
    assert_eq!(rep.hits, 10);
    assert!(c.stats.complete && c.stats.cdx_done == c.stats.cdx_total);
    o.max_hits = 4;
    let mut g = repo.grep(std::slice::from_ref(&h), o).await.unwrap();
    assert!(g.next().await.is_none());
    let rep = g.report().unwrap();
    assert_eq!(rep.hits, 4);
    assert!(rep.truncated);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn events_filter_each_kind() {
    let (_td, repo, h, located) = setup(30, |i| i % 2 == 1).await;
    hooks(&h).progress_lines.store(2, Ordering::Release);
    let target = located[5].clone();
    let dir = repo.path().join("data/repository").join(h.id().to_string());
    let seg = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(last_segment(&dir))
        .unwrap();
    let at = target.entry.offset + target.entry.raw_size / 2;
    let mut b = [0u8; 1];
    seg.read_exact_at(&mut b, at).unwrap();
    b[0] ^= 0xFF;
    seg.write_all_at(&b, at).unwrap();
    drop(seg);

    let run = |events: GrepEvents| {
        let (repo, h) = (repo.clone(), h.clone());
        async move {
            let mut o = opts(&["needle"]);
            o.events = events;
            let mut g = repo.grep(std::slice::from_ref(&h), o).await.unwrap();
            drain(&mut g).await
        }
    };
    let only = |hits, unreadable, progress| GrepEvents { hits, unreadable, progress };
    let d = run(only(true, false, false)).await;
    assert_eq!(d.hits.len(), 14);
    assert!(d.unreadable.is_empty() && d.progress.is_empty());
    assert_eq!(d.report.collections[0].stats.unreadable, 1);
    let d = run(only(false, true, false)).await;
    assert_eq!(d.unreadable.len(), 1);
    assert!(d.hits.is_empty() && d.progress.is_empty());
    let d = run(only(false, false, true)).await;
    assert!(d.hits.is_empty() && d.unreadable.is_empty());
    d.assert_progress(d.report.collections[0].stats.cdx_total);
    repo.shutdown().await.unwrap();
}
}

rt_test! {
async fn progress_monotonic_with_tombstones() {
    let (_td, repo, h, located) = setup(20, |_| true).await;
    assert_eq!(h.delete(located[..5].to_vec()).await.unwrap(), 5);
    hooks(&h).progress_lines.store(1, Ordering::Release);
    let total = h.stats().cdx_len - header_len(&repo, &h);
    let mut g = repo.grep(std::slice::from_ref(&h), opts(&["needle"])).await.unwrap();
    let d = drain(&mut g).await;
    let c = &d.report.collections[0];
    assert_eq!((c.stats.cdx_total, c.stats.cdx_done), (total, total));
    d.assert_progress(total);
    // One start event, then one per in-range line: 21 adds (warcinfo + 20) and 5 tombstones.
    assert_eq!(d.progress.len(), 1 + 21 + 5);
    repo.shutdown().await.unwrap();
}
}
