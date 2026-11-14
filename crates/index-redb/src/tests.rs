//! Crate-level tests: conformance suite, reopen, crash, concurrency, purge, differential.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use masstuffy_cdx::{Entry, Kind};
use masstuffy_index::conformance::{add, date, dump, key, purge_all};
use masstuffy_index::{
    Checkpoint, CollectionKey, Cursor, Error, MAX_LIMIT, MemIndex, Op, PatternQuery, Position,
    SecondaryIndex, UrlPattern, WarcDate,
};
use masstuffy_urlkey::{HostMatch, PathFilter, PortFilter};

use crate::testutil::{
    TestDir, TestIndex, check_consistency, counts, fresh, next_slot, snapshot, with,
};
use crate::{OpenError, Options, RedbIndex, SCHEMA_VERSION};

mod conformance {
    masstuffy_index::conformance_tests!(crate::testutil::fresh);
}

fn lazy(sync_every: u32) -> Options {
    Options {
        sync_every: std::num::NonZeroU32::new(sync_every).unwrap(),
        ..Options::default()
    }
}

/// Best-effort guard against livelocked loops (no timing assertions).
const TIMEOUT: Duration = Duration::from_secs(60);

fn p(segment: u32, offset: u64) -> Position {
    Position { segment, offset }
}

fn ins(c: CollectionKey, generation: u32, entry: Entry<'static>) -> Op<'static> {
    Op::Insert {
        collection: c,
        generation,
        entry,
    }
}

fn rep(c: CollectionKey, generation: u32) -> Op<'static> {
    Op::ReplaceGeneration {
        collection: c,
        generation,
    }
}

fn disc(c: CollectionKey, generation: u32) -> Op<'static> {
    Op::DiscardStaged {
        collection: c,
        generation,
    }
}

fn drop_c(c: CollectionKey) -> Op<'static> {
    Op::DropCollection { collection: c }
}

fn tomb(c: CollectionKey, generation: u32, position: Position) -> Op<'static> {
    Op::Tombstone {
        collection: c,
        generation,
        position,
    }
}

fn cp(c: CollectionKey, generation: u32, cdx_offset: u64) -> Op<'static> {
    Op::Checkpoint {
        collection: c,
        checkpoint: Checkpoint {
            generation,
            cdx_offset,
        },
    }
}

fn pat(host: &str, hm: HostMatch, pf: PortFilter, path: PathFilter<'_>) -> UrlPattern {
    UrlPattern::new(host, hm, pf, path).unwrap()
}

fn host_pat(host: &str) -> UrlPattern {
    pat(host, HostMatch::Exact, PortFilter::Any, PathFilter::Any)
}

/// Pages through a pattern with `limit`, returning every page.
fn pattern_pages(
    index: &dyn SecondaryIndex,
    c: CollectionKey,
    pattern: &UrlPattern,
    limit: usize,
) -> Vec<(Vec<masstuffy_index::Hit>, Option<Cursor>)> {
    let mut out = Vec::new();
    let mut q = PatternQuery::new(limit);
    for _ in 0..10_000 {
        let page = index.by_pattern(c, pattern, &q).unwrap();
        let next = page.next.clone();
        out.push((page.hits, page.next));
        match next {
            Some(n) => q = PatternQuery::new(limit).after(n),
            None => return out,
        }
    }
    panic!("pagination did not end");
}

/// Applies `ops` and checks consistency.
fn apply_checked(t: &TestIndex, ops: &[Op<'_>]) {
    t.index.apply(ops).unwrap();
    check_consistency(&t.index);
}

#[test]
fn consistency_after_flows() {
    let t = fresh();
    let (a, b) = (key(1), key(2));
    let e =
        |s, o, id: &str, url: &str| add(s, o, id, Some(url), date(2020, 1, 1, 0, 0, 0), "response");
    apply_checked(
        &t,
        &[
            rep(a, 1),
            ins(a, 1, e(1, 0, "a0", "http://x.test/0")),
            ins(a, 1, e(1, 10, "a1", "urn:uuid:1")),
            ins(a, 1, e(1, 20, "a2", "http://x.test/2")),
            rep(b, 1),
            ins(b, 1, e(1, 0, "b0", "http://y.test/0")),
            cp(a, 1, 5),
        ],
    );
    // Staged inserts, discard and restage in the same batch.
    apply_checked(
        &t,
        &[
            ins(a, 2, e(2, 0, "s0", "http://x.test/s0")),
            ins(a, 2, e(2, 10, "s1", "http://x.test/s1")),
            cp(a, 2, 7),
            disc(a, 2),
            ins(a, 2, e(2, 0, "s2", "http://x.test/s2")),
        ],
    );
    apply_checked(&t, &[tomb(a, 1, p(1, 10)), tomb(a, 1, p(9, 9))]);
    // Drop + recreate in one batch.
    apply_checked(
        &t,
        &[
            drop_c(b),
            rep(b, 1),
            ins(b, 1, e(1, 0, "b9", "http://y.test/9")),
        ],
    );
    apply_checked(&t, &[rep(a, 2)]);
    apply_checked(&t, &[disc(a, 3), drop_c(key(77))]);
    purge_all(&t);
    check_consistency(&t.index);
    let c = counts(&t.index);
    assert_eq!(c.dead, 0);
    assert_eq!(c.slots, c.live_gens);
    assert_eq!(t.stored_entries().unwrap(), 2);
    assert_eq!(dump(&t, a).len(), 1);
    assert_eq!(dump(&t, b).len(), 1);
}

#[test]
fn object_safe_and_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RedbIndex>();
    assert_send_sync::<OpenError>();
    let b: Box<dyn SecondaryIndex> = Box::new(fresh());
    assert!(b.collections().unwrap().is_empty());
}

fn answers(index: &dyn SecondaryIndex, c1: CollectionKey) -> String {
    let mut s = String::new();
    for i in 0..5 {
        s += &format!("{:?}\n", index.by_id(None, &format!("rid{i}")).unwrap());
    }
    let d = date(2020, 1, 1, 0, 0, 30);
    for url in ["http://h1.test/p4", "urn:uuid:5", "dns:h8.test"] {
        for dt in [None, Some(d)] {
            s += &format!("{:?}\n", index.by_url_nearest(c1, url, dt).unwrap());
        }
    }
    s += &format!("{:?}\n", pattern_pages(index, c1, &host_pat("h1.test"), 7));
    s += &format!("{:?}\n", index.stored_entries().unwrap());
    s
}

#[test]
fn reopen_after_drop_preserves_everything() {
    let t = fresh();
    let (c1, c2, c3, c4) = (key(1), key(2), key(3), key(4));
    let mut ops = vec![rep(c1, 1)];
    for i in 0..50u32 {
        let url = match i % 4 {
            0 => Some(format!("http://h1.test/p{i}")),
            1 => Some(format!("urn:uuid:{i}")),
            2 => Some(format!("dns:h{i}.test")),
            _ => None,
        };
        let wt = if i % 7 == 0 { "request" } else { "response" };
        ops.push(ins(
            c1,
            1,
            add(
                1,
                u64::from(i) * 100,
                &format!("rid{}", i % 20),
                url.as_deref(),
                date(2020, 1, 1, 0, 0, u8::try_from(i % 50).unwrap()),
                wt,
            ),
        ));
    }
    for i in [3u64, 10, 17, 24, 31] {
        ops.push(tomb(c1, 1, p(1, i * 100)));
    }
    ops.push(cp(c1, 1, 500));
    t.apply(&ops).unwrap();
    let e = |s, o, id: &str| {
        add(
            s,
            o,
            id,
            Some("http://z.test/"),
            date(2021, 1, 1, 0, 0, 0),
            "response",
        )
    };
    let mut ops = vec![rep(c2, 1)];
    for i in 0..5u64 {
        ops.push(ins(c2, 1, e(1, i * 10, &format!("c2a{i}"))));
    }
    for i in 0..10u64 {
        ops.push(ins(c2, 2, e(2, i * 10, &format!("c2s{i}"))));
    }
    ops.push(cp(c2, 2, 77));
    t.apply(&ops).unwrap();
    let mut ops = vec![rep(c3, 1)];
    for i in 0..10u64 {
        ops.push(ins(c3, 1, e(1, i * 10, &format!("c3{i}"))));
    }
    ops.push(rep(c3, 2));
    t.apply(&ops).unwrap();
    let mut ops = vec![rep(c4, 1)];
    for i in 0..5u64 {
        ops.push(ins(c4, 1, e(1, i * 10, &format!("c4{i}"))));
    }
    ops.push(drop_c(c4));
    t.apply(&ops).unwrap();
    check_consistency(&t.index);

    let cs = [c1, c2, c3, c4];
    let before = snapshot(&t, &cs);
    let ans = answers(&t, c1);
    let next_before = next_slot(&t.index);

    let t = t.reopen(&Options::default());
    assert_eq!(snapshot(&t, &cs), before);
    assert_eq!(answers(&t, c1), ans);
    check_consistency(&t.index);

    // Continue: staged rows become visible, new slots never collide with old ones.
    apply_checked(&t, &[rep(c2, 2)]);
    assert_eq!(dump(&t, c2).len(), 10);
    apply_checked(
        &t,
        &[
            rep(c1, 2),
            ins(c1, 2, e(5, 0, "n0")),
            ins(c1, 2, e(5, 10, "n1")),
        ],
    );
    assert!(next_slot(&t.index) > next_before);
    purge_all(&t);
    check_consistency(&t.index);
    let visible: usize = cs.iter().map(|c| dump(&t, *c).len()).sum();
    assert_eq!(visible, 12);
    assert_eq!(t.stored_entries().unwrap(), u64::try_from(visible).unwrap());
}

#[test]
fn reopen_after_drop_lazy_sync() {
    let o = lazy(1000);
    let t = with(&o);
    let c = key(1);
    t.apply(&[rep(c, 1)]).unwrap();
    for k in 0..20u64 {
        let ops: Vec<Op<'_>> = (0..3u64)
            .map(|i| {
                ins(
                    c,
                    1,
                    add(
                        u32::try_from(k).unwrap(),
                        i * 100,
                        &format!("r{k}-{i}"),
                        Some("http://l.test/"),
                        date(2020, 1, 1, 0, 0, 0),
                        "response",
                    ),
                )
            })
            .chain([cp(c, 1, k)])
            .collect();
        t.apply(&ops).unwrap();
    }
    assert!(t.index.unsynced() > 0);
    let t = t.reopen(&o);
    assert_eq!(dump(&t, c).len(), 60);
    assert_eq!(
        t.state(c).unwrap().unwrap().checkpoint.unwrap().cdx_offset,
        19
    );
    check_consistency(&t.index);
}

const CRASH_ENV: &str = "MASSTUFFY_INDEX_REDB_CRASH_DIR";

fn crash_batch(c: CollectionKey, generation: u32, segment: u32, cdx: u64) -> Vec<Op<'static>> {
    let mut ops: Vec<Op<'static>> = (0..10u64)
        .map(|i| {
            ins(
                c,
                generation,
                add(
                    segment,
                    i * 100,
                    &format!("crash-{segment}-{i}"),
                    Some("http://crash.test/"),
                    date(2020, 1, 1, 0, 0, 0),
                    "response",
                ),
            )
        })
        .collect();
    ops.push(cp(c, generation, cdx));
    ops
}

/// Runs only in the child process of `crash_keeps_consistent_prefix`; returns at once otherwise.
#[test]
#[ignore = "child process of crash_keeps_consistent_prefix"]
fn crash_child() {
    let Ok(dir) = std::env::var(CRASH_ENV) else {
        return;
    };
    let o = lazy(1000);
    let index = RedbIndex::open(std::path::Path::new(&dir).join("index.redb"), &o).unwrap();
    let c = key(1);
    index.apply(&[rep(c, 1)]).unwrap();
    for k in 1..=10u32 {
        index.apply(&crash_batch(c, 1, k, u64::from(k))).unwrap();
        if k == 5 {
            index.flush().unwrap();
        }
    }
    let mut ops = vec![rep(c, 2)];
    ops.extend(crash_batch(c, 2, 99, 99));
    index.apply(&ops).unwrap();
    for k in 100..=102u32 {
        index.apply(&crash_batch(c, 2, k, u64::from(k))).unwrap();
    }
    std::process::abort();
}

#[test]
fn crash_keeps_consistent_prefix() {
    let dir = TestDir::new();
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new(exe)
        .args(["tests::crash_child", "--exact", "--ignored", "--nocapture"])
        .env(CRASH_ENV, dir.path())
        .output()
        .expect("cannot spawn the crash child");
    assert!(!out.status.success(), "the child must abort");
    let index = RedbIndex::open(dir.file(), &Options::default()).unwrap();
    let c = key(1);
    let st = index.state(c).unwrap().unwrap();
    assert_eq!(st.active_generation, Some(2), "structural batch is durable");
    let k = st.checkpoint.unwrap().cdx_offset;
    assert!((99..=102).contains(&k), "checkpoint {k}");
    let mut expected_segments = vec![99u32];
    expected_segments.extend(100..=u32::try_from(k).unwrap());
    let hits = dump(&index, c);
    let mut segs: Vec<u32> = hits.iter().map(|h| h.entry.segment).collect();
    segs.dedup();
    assert_eq!(segs, expected_segments);
    assert_eq!(hits.len(), expected_segments.len() * 10);
    check_consistency(&index);
}

struct Done<'a>(&'a AtomicBool);
impl Drop for Done<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn concurrent_readers_during_large_apply() {
    const N: u64 = 10_000;
    let t = fresh();
    let c = key(1);
    t.apply(&[rep(c, 1)]).unwrap();
    let pattern = host_pat("big.test");
    let done = AtomicBool::new(false);
    let start = Instant::now();
    std::thread::scope(|s| {
        s.spawn(|| {
            let _g = Done(&done);
            let mut ops: Vec<Op<'_>> = (0..N)
                .map(|i| {
                    ins(
                        c,
                        1,
                        add(
                            1,
                            i * 100,
                            &format!("big-{i}"),
                            Some(&format!("http://big.test/{i}")),
                            date(2020, 1, 1, 0, 0, 0),
                            "response",
                        ),
                    )
                })
                .collect();
            ops.push(cp(c, 1, 1));
            t.apply(&ops).unwrap();
            let mut ops: Vec<Op<'_>> = (0..N).map(|i| tomb(c, 1, p(1, i * 100))).collect();
            ops.push(cp(c, 1, 2));
            t.apply(&ops).unwrap();
        });
        for _ in 0..4 {
            s.spawn(|| {
                let mut extra = 0;
                while extra < 20 {
                    if done.load(Ordering::SeqCst) {
                        extra += 1;
                    }
                    assert!(start.elapsed() < TIMEOUT, "livelock");
                    let cp_of = |t: &TestIndex| {
                        t.state(c)
                            .unwrap()
                            .and_then(|s| s.checkpoint)
                            .map(|c| c.cdx_offset)
                    };
                    let before = cp_of(&t);
                    let l = t.list(c, None, MAX_LIMIT).unwrap().len();
                    let pl = t
                        .by_pattern(c, &pattern, &PatternQuery::new(MAX_LIMIT))
                        .unwrap()
                        .hits
                        .len();
                    let after = cp_of(&t);
                    for n in [l, pl] {
                        match u64::try_from(n).unwrap() {
                            N => assert!(before <= Some(1) && after >= Some(1)),
                            0 => assert!(before.is_none() || after == Some(2)),
                            n => panic!("partial batch visible: {n}"),
                        }
                    }
                }
            });
        }
    });
    assert_eq!(t.list(c, None, MAX_LIMIT).unwrap().len(), 0);
    check_consistency(&t.index);
}

#[test]
fn purge_bounded_progress() {
    let t = fresh();
    let (c, d, e) = (key(1), key(2), key(3));
    let mut ops = vec![rep(c, 1)];
    for i in 0..1000u64 {
        ops.push(ins(
            c,
            1,
            add(
                1,
                i * 100,
                &format!("p{i}"),
                Some(&format!("http://pb.test/{i}")),
                date(2020, 1, 1, 0, 0, 0),
                "response",
            ),
        ));
    }
    t.apply(&ops).unwrap();
    let mut ops = vec![rep(c, 2)];
    for i in 0..10u64 {
        ops.push(ins(
            c,
            2,
            add(
                2,
                i * 100,
                &format!("q{i}"),
                Some(&format!("http://pb.test/g2/{i}")),
                date(2020, 1, 1, 0, 0, 0),
                "response",
            ),
        ));
    }
    // An empty dead slot from a dropped collection and one from a discarded staged slot.
    ops.extend([rep(d, 1), drop_c(d)]);
    ops.extend([
        rep(e, 1),
        ins(
            e,
            2,
            add(
                1,
                0,
                "tmp",
                Some("http://t.test/"),
                date(2020, 1, 1, 0, 0, 0),
                "response",
            ),
        ),
        tomb(e, 2, p(1, 0)),
        disc(e, 2),
    ]);
    t.apply(&ops).unwrap();
    assert_eq!(t.stored_entries().unwrap(), 1010);
    assert!(matches!(
        t.purge(0),
        Err(Error::InvalidLimit {
            limit: 0,
            max: usize::MAX
        })
    ));
    for round in 1..=10u64 {
        assert_eq!(t.purge(100).unwrap(), 100);
        assert_eq!(t.stored_entries().unwrap(), 1010 - round * 100);
        check_consistency(&t.index);
        assert_eq!(dump(&t, c).len(), 10);
    }
    assert_eq!(t.purge(100).unwrap(), 0);
    assert_eq!(t.stored_entries().unwrap(), 10);
    let n = counts(&t.index);
    assert_eq!(n.dead, 0);
    assert_eq!(n.slots, n.live_gens);
    check_consistency(&t.index);

    // Three dead slots of 30 rows each: the oldest is purged first.
    let t = fresh();
    let mut ops = Vec::new();
    for k in 1..=3u128 {
        ops.push(rep(key(k), 1));
        for i in 0..30u64 {
            ops.push(ins(
                key(k),
                1,
                add(
                    1,
                    i * 100,
                    &format!("r{k}-{i}"),
                    Some("http://d.test/"),
                    date(2020, 1, 1, 0, 0, 0),
                    "response",
                ),
            ));
        }
        ops.push(drop_c(key(k)));
    }
    t.apply(&ops).unwrap();
    assert_eq!(counts(&t.index).dead, 3);
    assert_eq!(t.purge(50).unwrap(), 50);
    assert_eq!(counts(&t.index).dead, 2);
    assert_eq!(counts(&t.index).entries, 40);
    check_consistency(&t.index);
    assert_eq!(t.purge(50).unwrap(), 40);
    assert_eq!(counts(&t.index).dead, 0);
    assert_eq!(t.purge(50).unwrap(), 0);
    assert_eq!(t.stored_entries().unwrap(), 0);
    let n = counts(&t.index);
    assert_eq!((n.entries, n.by_id, n.urls, n.slots), (0, 0, 0, 0));
}

#[test]
fn purge_concurrent_with_apply_and_readers() {
    const BATCH: u64 = 20;
    // Lazy sync keeps the test fast: it exercises the same interleavings without an fsync per
    // purge step.
    let o = lazy(1000);
    let t = with(&o);
    let (c1, c2) = (key(1), key(2));
    let mut ops = vec![rep(c1, 1), rep(c2, 1)];
    for i in 0..2000u64 {
        ops.push(ins(
            c1,
            1,
            add(
                1,
                i * 100,
                &format!("o{i}"),
                Some(&format!("http://old.test/{i}")),
                date(2020, 1, 1, 0, 0, 0),
                "response",
            ),
        ));
    }
    ops.push(rep(c1, 2));
    t.apply(&ops).unwrap();
    let done = AtomicBool::new(false);
    let start = Instant::now();
    std::thread::scope(|s| {
        s.spawn(|| {
            let _g = Done(&done);
            for b in 0..50u32 {
                let mut ops: Vec<Op<'_>> = (0..BATCH)
                    .map(|i| {
                        ins(
                            c2,
                            1,
                            add(
                                b,
                                i * 100,
                                &format!("n{b}-{i}"),
                                Some("http://new.test/"),
                                date(2020, 1, 1, 0, 0, 0),
                                "response",
                            ),
                        )
                    })
                    .collect();
                ops.push(cp(c2, 1, u64::from(b)));
                t.apply(&ops).unwrap();
            }
        });
        s.spawn(|| {
            loop {
                assert!(start.elapsed() < TIMEOUT, "livelock");
                let finished = done.load(Ordering::SeqCst);
                if t.purge(16).unwrap() == 0 && finished {
                    break;
                }
            }
        });
        s.spawn(|| {
            while !done.load(Ordering::SeqCst) {
                assert!(start.elapsed() < TIMEOUT, "livelock");
                let n = t.list(c2, None, MAX_LIMIT).unwrap().len();
                assert_eq!(n % usize::try_from(BATCH).unwrap(), 0);
            }
        });
    });
    assert_eq!(t.stored_entries().unwrap(), 50 * BATCH);
    assert_eq!(dump(&t, c2).len(), usize::try_from(50 * BATCH).unwrap());
    check_consistency(&t.index);
}

#[test]
fn schema_version_mismatch_is_reported() {
    let t = fresh();
    let TestIndex { index, dir } = t;
    drop(index);
    {
        let db = redb::Database::open(dir.file()).unwrap();
        let txn = db.begin_write().unwrap();
        {
            let mut meta = txn.open_table(crate::schema::META).unwrap();
            meta.insert(crate::schema::KEY_SCHEMA, 999u64).unwrap();
        }
        txn.commit().unwrap();
    }
    let err = RedbIndex::open(dir.file(), &Options::default()).unwrap_err();
    assert!(
        matches!(err, OpenError::SchemaMismatch { found: 999, expected } if expected == SCHEMA_VERSION),
        "{err:?}"
    );
    assert!(err.needs_rebuild());
}

#[test]
fn foreign_files_are_rejected() {
    // Not a redb file.
    let dir = TestDir::new();
    std::fs::write(dir.file(), vec![0xABu8; 8192]).unwrap();
    let err = RedbIndex::open(dir.file(), &Options::default()).unwrap_err();
    assert!(matches!(err, OpenError::Corrupt(_)), "{err:?}");
    assert!(err.needs_rebuild());

    // A redb file with another table only.
    let dir = TestDir::new();
    {
        let db = redb::Database::create(dir.file()).unwrap();
        let txn = db.begin_write().unwrap();
        {
            let def: redb::TableDefinition<&str, u64> = redb::TableDefinition::new("other");
            let mut t = txn.open_table(def).unwrap();
            t.insert("x", 1u64).unwrap();
        }
        txn.commit().unwrap();
    }
    let err = RedbIndex::open(dir.file(), &Options::default()).unwrap_err();
    assert!(matches!(err, OpenError::Corrupt(_)), "{err:?}");
    assert!(err.needs_rebuild());

    // A meta table without schema_version.
    let dir = TestDir::new();
    {
        let db = redb::Database::create(dir.file()).unwrap();
        let txn = db.begin_write().unwrap();
        {
            let mut t = txn.open_table(crate::schema::META).unwrap();
            t.insert("x", 1u64).unwrap();
        }
        txn.commit().unwrap();
    }
    let err = RedbIndex::open(dir.file(), &Options::default()).unwrap_err();
    assert!(matches!(err, OpenError::Corrupt(_)), "{err:?}");

    // An empty file is initialised.
    let dir = TestDir::new();
    std::fs::File::create(dir.file()).unwrap();
    let index = RedbIndex::open(dir.file(), &Options::default()).unwrap();
    assert!(index.collections().unwrap().is_empty());
}

#[test]
fn second_open_is_already_open() {
    let t = fresh();
    let err = RedbIndex::open(t.dir.file(), &Options::default()).unwrap_err();
    assert!(matches!(err, OpenError::AlreadyOpen), "{err:?}");
    assert!(!err.needs_rebuild());
    let t = t.reopen(&Options::default());
    assert!(t.collections().unwrap().is_empty());
}

#[test]
fn dangling_url_row_is_corrupt() {
    let t = fresh();
    let c = key(1);
    t.apply(&[
        rep(c, 1),
        ins(
            c,
            1,
            add(
                1,
                0,
                "x",
                Some("http://dang.test/"),
                date(2020, 1, 1, 0, 0, 0),
                "response",
            ),
        ),
    ])
    .unwrap();
    {
        let txn = t.index.db().begin_write().unwrap();
        {
            let mut entries = txn.open_table(crate::schema::ENTRIES).unwrap();
            let k = crate::codec::entry_key(1, p(1, 0));
            assert!(entries.remove(k.as_slice()).unwrap().is_some());
        }
        txn.commit().unwrap();
    }
    assert!(matches!(
        t.by_url_nearest(c, "http://dang.test/", None),
        Err(Error::Corrupt(_))
    ));
    assert!(matches!(
        t.by_pattern(c, &host_pat("dang.test"), &PatternQuery::new(10)),
        Err(Error::Corrupt(_))
    ));
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).unwrap()).unwrap()
    }
}

const POOL_URLS: [&str; 8] = [
    "http://a.test/x",
    "https://a.test/x",
    "http://a.test/x/y?q=1",
    "http://a.test:8080/x",
    "http://b.test/x",
    "http://b.test/z?k=v",
    "urn:uuid:one",
    "urn:uuid:two",
];

fn err_sig(e: &Error) -> String {
    match e {
        Error::InvalidEntry { op, .. } => format!("InvalidEntry{op}"),
        other => format!("{other:?}"),
    }
}

fn pool_dates() -> Vec<WarcDate> {
    (1..=5).map(|h| date(2020, 1, 1, h, 0, 0)).collect()
}

fn probe_dates() -> Vec<WarcDate> {
    let mut out = pool_dates();
    out.push(date(2020, 1, 1, 0, 0, 0));
    out.push(date(2020, 1, 1, 7, 0, 0));
    out.push(date(2020, 1, 1, 2, 0, 1));
    out.push(date(2020, 1, 1, 2, 59, 59));
    out
}

fn gen_batch(rng: &mut Rng, mem: &MemIndex) -> Vec<Op<'static>> {
    let dates = pool_dates();
    let n = 1 + rng.below(8);
    let mut ops = Vec::new();
    for _ in 0..n {
        let c = key(1 + u128::try_from(rng.below(3)).unwrap());
        let act = mem
            .state(c)
            .unwrap()
            .and_then(|s| s.active_generation)
            .unwrap_or(1);
        let g = match rng.below(20) {
            0 => act.saturating_sub(1).max(1),
            1..=5 => (act + 1).min(4),
            6 => (act + 2).min(4),
            _ => act,
        };
        ops.push(match rng.below(100) {
            0..=49 => {
                let url = match rng.below(9) {
                    8 => None,
                    i => Some(POOL_URLS[i]),
                };
                let wt = ["response", "request", "metadata"][rng.below(3)];
                let e = add(
                    u32::try_from(1 + rng.below(2)).unwrap(),
                    u64::try_from(rng.below(5)).unwrap() * 100,
                    &format!("id{}", rng.below(10)),
                    url,
                    dates[rng.below(5)],
                    wt,
                );
                ins(c, g, e)
            }
            50..=64 => tomb(
                c,
                g,
                p(
                    u32::try_from(1 + rng.below(2)).unwrap(),
                    u64::try_from(rng.below(5)).unwrap() * 100,
                ),
            ),
            65..=72 => rep(c, g),
            73..=77 => disc(c, g),
            78..=80 => drop_c(c),
            _ => cp(c, g, u64::try_from(rng.below(1000)).unwrap()),
        });
    }
    if rng.below(10) == 0 {
        let at = rng.below(ops.len() + 1);
        let mut e = add(1, 0, "bad", None, dates[0], "response");
        match rng.below(3) {
            0 => e.raw_size = 0,
            1 => e.kind = Kind::Tombstone,
            _ => e.record_id = std::borrow::Cow::Borrowed(""),
        }
        ops.insert(at, ins(key(1), 1, e));
    }
    ops
}

fn compare(mem: &MemIndex, red: &dyn SecondaryIndex, full: bool) {
    let cs = [key(1), key(2), key(3)];
    assert_eq!(mem.collections().unwrap(), red.collections().unwrap());
    for c in cs {
        assert_eq!(mem.state(c).unwrap(), red.state(c).unwrap(), "state {c}");
        assert_eq!(dump(mem, c), dump(red, c), "dump {c}");
    }
    for i in 0..10 {
        let id = format!("id{i}");
        assert_eq!(
            mem.by_id(None, &id).unwrap(),
            red.by_id(None, &id).unwrap(),
            "by_id {id}"
        );
        for c in cs {
            assert_eq!(
                mem.by_id(Some(c), &id).unwrap(),
                red.by_id(Some(c), &id).unwrap(),
                "by_id {id} {c}"
            );
        }
    }
    let pats = [
        host_pat("a.test"),
        pat(
            "test",
            HostMatch::WithSubdomains,
            PortFilter::Any,
            PathFilter::Any,
        ),
        pat(
            "a.test",
            HostMatch::Exact,
            PortFilter::Any,
            PathFilter::Prefix("/x"),
        ),
        pat(
            "a.test",
            HostMatch::Exact,
            PortFilter::Any,
            PathFilter::Exact("/x"),
        ),
        pat(
            "a.test",
            HostMatch::Exact,
            PortFilter::Default,
            PathFilter::Any,
        ),
        pat(
            "a.test",
            HostMatch::Exact,
            PortFilter::Explicit(8080),
            PathFilter::Any,
        ),
    ];
    for c in cs {
        for pt in &pats {
            assert_eq!(
                pattern_pages(mem, c, pt, 2),
                pattern_pages(red, c, pt, 2),
                "pattern {c}"
            );
        }
    }
    let probes = probe_dates();
    for c in cs {
        for url in POOL_URLS {
            let dates: Vec<Option<WarcDate>> = if full {
                std::iter::once(None)
                    .chain(probes.iter().copied().map(Some))
                    .collect()
            } else {
                vec![None, Some(probes[2])]
            };
            for d in dates {
                assert_eq!(
                    mem.by_url_nearest(c, url, d).unwrap(),
                    red.by_url_nearest(c, url, d).unwrap(),
                    "nearest {c} {url} {d:?}"
                );
            }
        }
    }
}

#[test]
fn differential_against_mem_index() {
    for seed in [0x1234_5678_9ABC_DEF1u64, 42, 0xDEAD_BEEF_CAFE_F00D] {
        let mut rng = Rng(seed);
        let mem = MemIndex::new();
        let red = fresh();
        let mut ok = 0;
        let mut err = 0;
        for i in 0..150 {
            let ops = gen_batch(&mut rng, &mem);
            let (a, b) = (mem.apply(&ops), red.apply(&ops));
            match (&a, &b) {
                (Ok(()), Ok(())) => ok += 1,
                (Err(x), Err(y)) => {
                    assert_eq!(err_sig(x), err_sig(y), "batch {i}: {ops:?}");
                    err += 1;
                }
                _ => panic!("batch {i} diverged: mem {a:?} redb {b:?}\n{ops:?}"),
            }
            compare(&mem, &red, i % 5 == 0);
            if i % 20 == 19 {
                red.purge(3).unwrap();
                check_consistency(&red.index);
            }
        }
        assert!(ok > 40 && err > 0, "ok {ok} err {err}");
        purge_all(&red);
        check_consistency(&red.index);
        assert_eq!(mem.stored_entries().unwrap(), red.stored_entries().unwrap());
        compare(&mem, &red, true);
    }
}

#[test]
fn bulk_20k_then_queries() {
    let t = fresh();
    let c = key(1);
    t.apply(&[rep(c, 1)]).unwrap();
    let start = Instant::now();
    let mut n = 0u64;
    for batch in 0..20u64 {
        let mut ops: Vec<Op<'_>> = Vec::with_capacity(1001);
        for _ in 0..1000 {
            let (host, path, dt) = (n / 200, (n / 2) % 100, n % 2);
            ops.push(ins(
                c,
                1,
                add(
                    1,
                    n * 100,
                    &format!("bulk-{n}"),
                    Some(&format!("http://h{host}.test/p{path}")),
                    date(2020, 1, 1, u8::try_from(dt).unwrap(), 0, 0),
                    "response",
                ),
            ));
            n += 1;
        }
        ops.push(cp(c, 1, batch));
        t.apply(&ops).unwrap();
    }
    eprintln!("bulk insert 20k: {:?}", start.elapsed());
    let start = Instant::now();
    let mut after = None;
    let mut total = 0;
    loop {
        let page = t.list(c, after, MAX_LIMIT).unwrap();
        total += page.len();
        match page.last() {
            Some(h) if page.len() == MAX_LIMIT => after = Some(h.position()),
            _ => break,
        }
    }
    assert_eq!(total, 20_000);
    for i in 0..1000u64 {
        let h = t.by_id(None, &format!("bulk-{}", i * 20)).unwrap().unwrap();
        assert_eq!(h.entry.offset, i * 20 * 100);
        let (host, path) = (i % 100, i % 100);
        let url = format!("http://h{host}.test/p{path}");
        assert!(t.by_url_nearest(c, &url, None).unwrap().is_some());
    }
    let pages = pattern_pages(&t, c, &host_pat("h7.test"), 100);
    assert_eq!(pages.iter().map(|p| p.0.len()).sum::<usize>(), 200);
    assert!(pages.last().unwrap().1.is_none());
    for h in 0..100 {
        let page = t
            .by_pattern(c, &host_pat(&format!("h{h}.test")), &PatternQuery::new(100))
            .unwrap();
        assert_eq!(page.hits.len(), 100);
        assert!(page.next.is_some());
    }
    eprintln!("bulk queries: {:?}", start.elapsed());
}

#[test]
fn compact_after_purge() {
    let mut t = fresh();
    let c = key(1);
    let mut ops = vec![rep(c, 1)];
    for i in 0..5000u64 {
        ops.push(ins(
            c,
            1,
            add(
                1,
                i * 100,
                &format!("cp{i}"),
                Some(&format!("http://cmp.test/{i}")),
                date(2020, 1, 1, 0, 0, 0),
                "response",
            ),
        ));
    }
    ops.push(rep(c, 2));
    ops.push(ins(
        c,
        2,
        add(
            2,
            0,
            "keep",
            Some("http://keep.test/"),
            date(2020, 1, 1, 0, 0, 0),
            "response",
        ),
    ));
    t.apply(&ops).unwrap();
    purge_all(&t);
    let snap = snapshot(&t, &[c]);
    let before = std::fs::metadata(t.dir.file()).unwrap().len();
    t.index.compact().unwrap();
    let after = std::fs::metadata(t.dir.file()).unwrap().len();
    assert!(after <= before, "{after} > {before}");
    assert_eq!(snapshot(&t, &[c]), snap);
    check_consistency(&t.index);
}

#[test]
fn flush_is_noop_when_synced() {
    let t = fresh();
    t.apply(&[rep(key(1), 1)]).unwrap();
    assert_eq!(t.index.unsynced(), 0);
    t.index.flush().unwrap();
    assert_eq!(t.index.unsynced(), 0);

    let o = lazy(100);
    let t = with(&o);
    t.apply(&[cp(key(1), 1, 1)]).unwrap();
    assert_eq!(t.index.unsynced(), 1);
    t.index.flush().unwrap();
    assert_eq!(t.index.unsynced(), 0);
}

#[test]
fn by_id_nul_query_and_stricter_scan() {
    let t = fresh();
    let c = key(1);
    t.apply(&[
        rep(c, 1),
        ins(
            c,
            1,
            add(1, 0, "a", None, date(2020, 1, 1, 0, 0, 0), "response"),
        ),
        ins(
            c,
            1,
            add(1, 100, "ab", None, date(2020, 1, 1, 0, 0, 0), "response"),
        ),
    ])
    .unwrap();
    assert!(t.by_id(None, "a\0\0\0\0\0\0\0").unwrap().is_none());
    assert_eq!(t.by_id(None, "a").unwrap().unwrap().entry.offset, 0);
    assert_eq!(t.by_id(None, "ab").unwrap().unwrap().entry.offset, 100);
}

#[test]
fn flush_makes_interleaved_lazy_commits_durable() {
    let o = lazy(1000);
    let t = with(&o);
    let c = key(1);
    let mut ops = vec![rep(c, 1), rep(key(2), 1)];
    for i in 0..200u64 {
        ops.push(ins(
            key(2),
            1,
            add(
                1,
                i * 100,
                &format!("o{i}"),
                Some("http://o.test/"),
                date(2020, 1, 1, 0, 0, 0),
                "response",
            ),
        ));
    }
    ops.push(drop_c(key(2)));
    t.apply(&ops).unwrap();
    std::thread::scope(|s| {
        s.spawn(|| {
            for b in 0..40u32 {
                let ops: Vec<Op<'_>> = (0..5u64)
                    .map(|i| {
                        ins(
                            c,
                            1,
                            add(
                                b,
                                i * 100,
                                &format!("n{b}-{i}"),
                                Some("http://n.test/"),
                                date(2020, 1, 1, 0, 0, 0),
                                "response",
                            ),
                        )
                    })
                    .collect();
                t.apply(&ops).unwrap();
            }
        });
        s.spawn(|| {
            for _ in 0..60 {
                t.purge(4).unwrap();
            }
        });
    });
    t.index.flush().unwrap();
    assert_eq!(t.index.unsynced(), 0);
    let t = t.reopen(&o);
    assert_eq!(dump(&t, c).len(), 200);
    check_consistency(&t.index);
}
