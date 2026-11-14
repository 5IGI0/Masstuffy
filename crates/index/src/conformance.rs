//! Conformance suite every [`SecondaryIndex`] implementation runs against itself.
//!
//! Use the [`conformance_tests!`](crate::conformance_tests) macro:
//! `mod conformance { masstuffy_index::conformance_tests!(MyIndex::fresh); }`. The factory is called
//! once per fresh index a case needs; a backend that needs a temporary directory passes a closure
//! returning a wrapper that owns the directory and implements `SecondaryIndex` by delegation.

// Test-support code: failures are assertion panics. It is never compiled into production builds
// unless the `conformance` feature is enabled, which only happens from dev-dependencies. The
// allowances are scoped to this module; library code stays strict.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use masstuffy_cdx::{Entry, Kind};
use masstuffy_urlkey::{HostMatch, PathFilter, PortFilter, UrlPattern};
use masstuffy_warc::WarcDate;

use crate::{
    Checkpoint, CollectionKey, CollectionState, Cursor, Error, Hit, MAX_LIMIT, Op, PatternQuery,
    Position, SecondaryIndex,
};

/// Collection key from a number.
pub fn key(n: u128) -> CollectionKey {
    CollectionKey::new(n)
}

/// Date with zero nanoseconds.
pub fn date(y: u16, mo: u8, d: u8, h: u8, mi: u8, s: u8) -> WarcDate {
    WarcDate::new(y, mo, d, h, mi, s, 0).unwrap()
}

/// A valid `Add` entry (`raw_size` 10, `digest` 0).
pub fn add(
    segment: u32,
    offset: u64,
    record_id: &str,
    url: Option<&str>,
    date: WarcDate,
    warc_type: &str,
) -> Entry<'static> {
    Entry {
        kind: Kind::Add,
        segment,
        offset,
        raw_size: 10,
        digest: 0,
        date,
        warc_type: Cow::Owned(warc_type.to_owned()),
        record_id: Cow::Owned(record_id.to_owned()),
        url: url.map(|u| Cow::Owned(u.to_owned())),
    }
}

/// Every visible entry of `c` in position order, paging through `list` with limit 3.
pub fn dump<I: SecondaryIndex + ?Sized>(index: &I, c: CollectionKey) -> Vec<Hit> {
    let mut out: Vec<Hit> = Vec::new();
    loop {
        let after = out.last().map(Hit::position);
        let page = index.list(c, after, 3).unwrap();
        let n = page.len();
        out.extend(page);
        if n < 3 {
            return out;
        }
    }
}

/// Calls `purge(64)` until it returns 0.
pub fn purge_all<I: SecondaryIndex + ?Sized>(index: &I) {
    for _ in 0..1_000_000 {
        if index.purge(64).unwrap() == 0 {
            return;
        }
    }
    panic!("purge did not converge");
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

/// Sets `done` on drop so reader threads stop even if the writer panics.
struct Done<'a>(&'a AtomicBool);
impl Drop for Done<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Best-effort guard: `thread::scope` joins every thread before returning, so this only stops
/// livelocked loops; it cannot fire on a deadlock.
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
fn tomb(c: CollectionKey, generation: u32, position: Position) -> Op<'static> {
    Op::Tombstone {
        collection: c,
        generation,
        position,
    }
}
fn rep(c: CollectionKey, generation: u32) -> Op<'static> {
    Op::ReplaceGeneration {
        collection: c,
        generation,
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
fn resp(segment: u32, offset: u64, id: &str, url: &str, d: WarcDate) -> Entry<'static> {
    add(segment, offset, id, Some(url), d, "response")
}
fn pat(host: &str, hm: HostMatch, pf: PortFilter, path: PathFilter<'_>) -> UrlPattern {
    UrlPattern::new(host, hm, pf, path).unwrap()
}
fn host_any(host: &str) -> UrlPattern {
    pat(host, HostMatch::Exact, PortFilter::Any, PathFilter::Any)
}
fn query<I: SecondaryIndex + ?Sized>(
    index: &I,
    c: CollectionKey,
    pattern: &UrlPattern,
) -> Vec<Hit> {
    index
        .by_pattern(c, pattern, &PatternQuery::new(MAX_LIMIT))
        .unwrap()
        .hits
}
fn urls(hits: &[Hit]) -> Vec<String> {
    hits.iter()
        .map(|h| h.entry.url.as_deref().unwrap_or("-").to_owned())
        .collect()
}
fn sorted_urls(hits: &[Hit]) -> Vec<String> {
    let mut v = urls(hits);
    v.sort();
    v
}
fn ids(hits: &[Hit]) -> Vec<String> {
    hits.iter().map(|h| h.entry.record_id.to_string()).collect()
}
/// A fresh index with generation 1 of `c` active.
fn active<I: SecondaryIndex>(new: &dyn Fn() -> I, c: CollectionKey) -> I {
    let i = new();
    i.apply(&[rep(c, 1)]).unwrap();
    i
}
fn nearest_id<I: SecondaryIndex>(
    i: &I,
    c: CollectionKey,
    url: &str,
    d: Option<WarcDate>,
) -> Option<String> {
    i.by_url_nearest(c, url, d)
        .unwrap()
        .map(|h| h.entry.record_id.to_string())
}

/// Empty index: every query is empty.
pub fn empty_index<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let i = new();
    let c = key(1);
    assert_eq!(i.state(c).unwrap(), None);
    assert!(i.collections().unwrap().is_empty());
    assert_eq!(i.stored_entries().unwrap(), 0);
    assert!(i.by_id(None, "x").unwrap().is_none());
    assert!(i.by_id(Some(c), "x").unwrap().is_none());
    assert!(
        i.by_url_nearest(c, "http://a.test/", None)
            .unwrap()
            .is_none()
    );
    let page = i
        .by_pattern(c, &host_any("a.test"), &PatternQuery::default())
        .unwrap();
    assert!(page.hits.is_empty() && page.next.is_none());
    assert!(i.list(c, None, 10).unwrap().is_empty());
    i.apply(&[]).unwrap();
    assert_eq!(i.purge(1).unwrap(), 0);
}

/// Staged entries are invisible until `ReplaceGeneration`.
pub fn insert_invisible_until_activation<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let i = new();
    let c = key(1);
    i.apply(&[ins(
        c,
        1,
        resp(1, 0, "a", "http://a.test/", date(2020, 1, 1, 0, 0, 0)),
    )])
    .unwrap();
    assert!(i.by_id(None, "a").unwrap().is_none());
    assert!(i.list(c, None, 10).unwrap().is_empty());
    assert_eq!(i.state(c).unwrap(), Some(CollectionState::default()));
    assert_eq!(i.stored_entries().unwrap(), 1);
    i.apply(&[rep(c, 1)]).unwrap();
    assert!(i.by_id(None, "a").unwrap().is_some());
    assert_eq!(dump(&i, c).len(), 1);
}

/// `by_id` scoping and hit contents.
pub fn by_id_scope<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let (c, d) = (key(1), key(2));
    let i = new();
    let e = resp(1, 0, "x", "http://a.test/", date(2020, 1, 1, 0, 0, 0));
    i.apply(&[
        rep(c, 1),
        rep(d, 1),
        ins(c, 1, e.clone()),
        ins(d, 1, e.clone()),
    ])
    .unwrap();
    let h = i.by_id(Some(c), "x").unwrap().unwrap();
    assert_eq!((h.collection, h.generation, h.entry), (c, 1, e));
    assert!(i.by_id(Some(key(3)), "x").unwrap().is_none());
    assert!(i.by_id(None, "missing").unwrap().is_none());
}

/// `by_id` returns the most recently committed copy.
pub fn by_id_most_recent_commit<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let (c, d) = (key(1), key(2));
    let i = active(new, c);
    i.apply(&[
        ins(
            c,
            1,
            resp(1, 0, "x", "http://a.test/", date(2024, 1, 1, 0, 0, 0)),
        ),
        ins(
            c,
            1,
            resp(1, 100, "x", "http://a.test/", date(2010, 1, 1, 0, 0, 0)),
        ),
        ins(
            c,
            1,
            resp(2, 0, "x", "http://a.test/", date(2001, 1, 1, 0, 0, 0)),
        ),
    ])
    .unwrap();
    assert_eq!(i.by_id(Some(c), "x").unwrap().unwrap().position(), p(2, 0));
    assert_eq!(i.by_id(None, "x").unwrap().unwrap().position(), p(2, 0));

    let j = new();
    j.apply(&[
        rep(c, 1),
        rep(d, 1),
        ins(
            c,
            1,
            resp(1, 0, "x", "http://a.test/", date(2010, 1, 1, 0, 0, 0)),
        ),
        ins(
            d,
            1,
            resp(1, 0, "x", "http://a.test/", date(2015, 1, 1, 0, 0, 0)),
        ),
    ])
    .unwrap();
    assert_eq!(j.by_id(None, "x").unwrap().unwrap().collection, d);
    let k = new();
    k.apply(&[
        rep(c, 1),
        rep(d, 1),
        ins(
            c,
            1,
            resp(1, 0, "x", "http://a.test/", date(2015, 1, 1, 0, 0, 0)),
        ),
        ins(
            d,
            1,
            resp(1, 0, "x", "http://a.test/", date(2015, 1, 1, 0, 0, 0)),
        ),
    ])
    .unwrap();
    assert_eq!(k.by_id(None, "x").unwrap().unwrap().collection, d);
}

/// Tombstoning the latest copy falls back to the previous one.
pub fn by_id_tombstone_falls_back<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let i = active(new, c);
    i.apply(&[
        ins(
            c,
            1,
            resp(1, 0, "x", "http://a.test/", date(2024, 1, 1, 0, 0, 0)),
        ),
        ins(
            c,
            1,
            resp(1, 100, "x", "http://a.test/", date(2010, 1, 1, 0, 0, 0)),
        ),
        ins(
            c,
            1,
            resp(2, 0, "x", "http://a.test/", date(2001, 1, 1, 0, 0, 0)),
        ),
    ])
    .unwrap();
    i.apply(&[tomb(c, 1, p(2, 0))]).unwrap();
    assert_eq!(i.by_id(None, "x").unwrap().unwrap().position(), p(1, 100));
    i.apply(&[tomb(c, 1, p(1, 100)), tomb(c, 1, p(1, 0))])
        .unwrap();
    assert!(i.by_id(None, "x").unwrap().is_none());
}

/// The same record id at two locators gives two entries.
pub fn same_record_id_distinct_copies<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let i = active(new, c);
    i.apply(&[
        ins(
            c,
            1,
            resp(1, 0, "x", "http://a.test/", date(2020, 1, 1, 0, 0, 0)),
        ),
        ins(
            c,
            1,
            resp(1, 10, "x", "http://a.test/", date(2020, 1, 1, 0, 0, 0)),
        ),
    ])
    .unwrap();
    assert_eq!(dump(&i, c).len(), 2);
}

fn three_dates<I: SecondaryIndex>(new: &dyn Fn() -> I) -> (I, CollectionKey) {
    let c = key(1);
    let i = active(new, c);
    i.apply(&[
        ins(
            c,
            1,
            resp(
                1,
                0,
                "d2010",
                "http://example.com/a",
                date(2010, 1, 1, 0, 0, 0),
            ),
        ),
        ins(
            c,
            1,
            resp(
                1,
                10,
                "d2015",
                "http://example.com/a",
                date(2015, 1, 1, 0, 0, 0),
            ),
        ),
        ins(
            c,
            1,
            resp(
                1,
                20,
                "d2020",
                "http://example.com/a",
                date(2020, 1, 1, 0, 0, 0),
            ),
        ),
    ])
    .unwrap();
    (i, c)
}

/// Target equal to a capture date returns that capture.
pub fn nearest_exact_match<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let (i, c) = three_dates(new);
    let r = nearest_id(
        &i,
        c,
        "http://example.com/a",
        Some(date(2015, 1, 1, 0, 0, 0)),
    );
    assert_eq!(r.as_deref(), Some("d2015"));
}

/// Nearest before/after, including out-of-range targets.
pub fn nearest_before_after<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let (i, c) = three_dates(new);
    for (y, expected) in [
        (2016, "d2015"),
        (2019, "d2020"),
        (1990, "d2010"),
        (2099, "d2020"),
    ] {
        let r = nearest_id(&i, c, "http://example.com/a", Some(date(y, 1, 1, 0, 0, 0)));
        assert_eq!(r.as_deref(), Some(expected), "{y}");
    }
}

/// Equal distance prefers the later capture.
pub fn nearest_tie_prefers_later<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let i = active(new, c);
    i.apply(&[
        ins(
            c,
            1,
            resp(1, 0, "early", "http://a.test/", date(2020, 1, 1, 12, 0, 0)),
        ),
        ins(
            c,
            1,
            resp(1, 10, "late", "http://a.test/", date(2020, 1, 1, 12, 0, 10)),
        ),
    ])
    .unwrap();
    let r = nearest_id(&i, c, "http://a.test/", Some(date(2020, 1, 1, 12, 0, 5)));
    assert_eq!(r.as_deref(), Some("late"));
}

/// Identical dates: the latest commit wins.
pub fn nearest_same_date_prefers_latest_commit<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let i = active(new, c);
    let d = date(2020, 1, 1, 0, 0, 0);
    i.apply(&[
        ins(c, 1, resp(1, 50, "second", "http://a.test/", d)),
        ins(c, 1, resp(1, 0, "first", "http://a.test/", d)),
    ])
    .unwrap();
    assert_eq!(
        nearest_id(&i, c, "http://a.test/", Some(d)).as_deref(),
        Some("second")
    );
    assert_eq!(
        nearest_id(&i, c, "http://a.test/", None).as_deref(),
        Some("second")
    );
}

/// Without a date the latest capture is returned.
pub fn nearest_without_date_is_latest<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let (i, c) = three_dates(new);
    let r = nearest_id(&i, c, "http://example.com/a", None);
    assert_eq!(r.as_deref(), Some("d2020"));
}

/// `request` records are never returned by URL lookup.
pub fn nearest_excludes_request<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let i = active(new, c);
    let u = "http://a.test/";
    i.apply(&[
        ins(
            c,
            1,
            add(1, 0, "req", Some(u), date(2020, 1, 1, 0, 0, 0), "request"),
        ),
        ins(c, 1, resp(1, 10, "resp", u, date(2020, 1, 2, 0, 0, 0))),
    ])
    .unwrap();
    assert_eq!(
        nearest_id(&i, c, u, Some(date(2020, 1, 1, 0, 0, 0))).as_deref(),
        Some("resp")
    );
    let j = active(new, c);
    j.apply(&[ins(
        c,
        1,
        add(1, 0, "req", Some(u), date(2020, 1, 1, 0, 0, 0), "request"),
    )])
    .unwrap();
    assert_eq!(nearest_id(&j, c, u, None), None);
    j.apply(&[ins(
        c,
        1,
        add(
            1,
            10,
            "meta",
            Some(u),
            date(2020, 1, 1, 0, 0, 0),
            "metadata",
        ),
    )])
    .unwrap();
    assert_eq!(nearest_id(&j, c, u, None).as_deref(), Some("meta"));
}

/// http(s) URLs are compared by massaged key.
pub fn nearest_url_normalised<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let i = active(new, c);
    i.apply(&[ins(
        c,
        1,
        resp(
            1,
            0,
            "s",
            "https://www.Example.com:443/p?b=2&a=1",
            date(2020, 1, 1, 0, 0, 0),
        ),
    )])
    .unwrap();
    for q in [
        "http://www.example.com/p?a=1&b=2",
        "HTTPS://WWW.EXAMPLE.COM/p?a=1&b=2",
    ] {
        assert!(nearest_id(&i, c, q, None).is_some(), "{q}");
    }
    for q in [
        "http://example.com/p?a=1&b=2",
        "https://www.example.com/P?a=1&b=2",
    ] {
        assert!(nearest_id(&i, c, q, None).is_none(), "{q}");
    }
}

/// Non-http URLs use exact equality, and entries without URL are never found by URL.
pub fn nearest_raw_urls_exact<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let i = active(new, c);
    let d = date(2020, 1, 1, 0, 0, 0);
    i.apply(&[
        ins(c, 1, resp(1, 0, "u", "urn:x:1", d)),
        ins(c, 1, resp(1, 10, "n", "dns:example.com", d)),
        ins(c, 1, add(1, 20, "none", None, d, "response")),
    ])
    .unwrap();
    assert_eq!(nearest_id(&i, c, "urn:x:1", None).as_deref(), Some("u"));
    assert_eq!(
        nearest_id(&i, c, "dns:example.com", None).as_deref(),
        Some("n")
    );
    assert_eq!(nearest_id(&i, c, "urn:x:2", None), None);
    assert_eq!(nearest_id(&i, c, "URN:x:1", None), None);
}

/// URL lookup is scoped to the collection.
pub fn nearest_scoped_to_collection<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let (c, d) = (key(1), key(2));
    let i = new();
    let u = "http://a.test/";
    i.apply(&[
        rep(c, 1),
        rep(d, 1),
        ins(c, 1, resp(1, 0, "inc", u, date(2020, 1, 1, 0, 0, 0))),
        ins(d, 1, resp(1, 0, "ind", u, date(2021, 1, 1, 0, 0, 0))),
    ])
    .unwrap();
    assert_eq!(nearest_id(&i, c, u, None).as_deref(), Some("inc"));
    assert_eq!(nearest_id(&i, d, u, None).as_deref(), Some("ind"));
}

/// A tombstone hides the entry from every query; re-insert restores it.
pub fn tombstone_hides_everywhere<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let i = active(new, c);
    let u = "http://a.test/";
    let e = resp(1, 0, "x", u, date(2020, 1, 1, 0, 0, 0));
    i.apply(&[ins(c, 1, e.clone())]).unwrap();
    i.apply(&[tomb(c, 1, p(1, 0))]).unwrap();
    assert!(i.by_id(None, "x").unwrap().is_none());
    assert!(i.by_url_nearest(c, u, None).unwrap().is_none());
    assert!(query(&i, c, &host_any("a.test")).is_empty());
    assert!(dump(&i, c).is_empty());
    i.apply(&[tomb(c, 1, p(9, 9))]).unwrap();
    i.apply(&[ins(c, 1, e)]).unwrap();
    assert!(i.by_id(None, "x").unwrap().is_some());
    assert_eq!(dump(&i, c).len(), 1);
}

/// Inserting an existing identity is a no-op.
pub fn insert_duplicate_identity_is_noop<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let i = active(new, c);
    let e = resp(1, 0, "x", "http://a.test/", date(2020, 1, 1, 0, 0, 0));
    i.apply(&[ins(c, 1, e.clone()), ins(c, 1, e.clone())])
        .unwrap();
    i.apply(&[ins(c, 1, e)]).unwrap();
    assert_eq!(dump(&i, c).len(), 1);
    assert_eq!(i.stored_entries().unwrap(), 1);
}

/// Generation replacement is one atomic switch.
pub fn replace_generation_switches_atomically<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let i = active(new, c);
    let d = date(2020, 1, 1, 0, 0, 0);
    let (ua, ub) = ("http://g.test/a", "http://g.test/b");
    i.apply(&[
        ins(c, 1, resp(1, 0, "a", ua, d)),
        ins(c, 1, resp(1, 10, "b", ub, d)),
    ])
    .unwrap();
    i.apply(&[cp(c, 1, 5)]).unwrap();
    i.apply(&[ins(c, 2, resp(2, 0, "a2", ua, d)), cp(c, 2, 20)])
        .unwrap();
    i.apply(&[ins(c, 2, resp(2, 10, "b2", ub, d)), cp(c, 2, 40)])
        .unwrap();
    assert_eq!(ids(&dump(&i, c)), ["a", "b"]);
    let during = i.state(c).unwrap().unwrap();
    assert_eq!(
        during.checkpoint,
        Some(Checkpoint {
            generation: 1,
            cdx_offset: 5
        })
    );
    assert_eq!(
        during.staged_checkpoint,
        Some(Checkpoint {
            generation: 2,
            cdx_offset: 40
        })
    );
    i.apply(&[rep(c, 2), cp(c, 2, 77)]).unwrap();
    let hits = dump(&i, c);
    assert_eq!(ids(&hits), ["a2", "b2"]);
    assert!(hits.iter().all(|h| h.generation == 2));
    assert!(i.by_id(None, "a").unwrap().is_none());
    assert_eq!(i.by_id(None, "a2").unwrap().unwrap().generation, 2);
    assert_eq!(nearest_id(&i, c, ua, None).as_deref(), Some("a2"));
    assert_eq!(ids(&query(&i, c, &host_any("g.test"))), ["a2", "b2"]);
    assert_eq!(
        i.state(c).unwrap(),
        Some(CollectionState {
            active_generation: Some(2),
            checkpoint: Some(Checkpoint {
                generation: 2,
                cdx_offset: 77
            }),
            staged_checkpoint: None,
        })
    );
    purge_all(&i);
    assert_eq!(i.stored_entries().unwrap(), 2);
}

/// No-op, stale and staged-generation rules.
pub fn replace_generation_rules<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let i = active(new, c);
    let d = date(2020, 1, 1, 0, 0, 0);
    let u = "http://a.test/";
    i.apply(&[
        ins(c, 1, resp(1, 0, "e1", u, d)),
        ins(c, 2, resp(2, 0, "e2", u, d)),
        ins(c, 3, resp(3, 0, "e3", u, d)),
        cp(c, 1, 5),
    ])
    .unwrap();
    i.apply(&[rep(c, 2)]).unwrap();
    i.apply(&[rep(c, 2)]).unwrap();
    let stale = |r: Result<(), Error>| {
        assert!(
            matches!(
                r,
                Err(Error::StaleGeneration {
                    op: 0,
                    generation: 1,
                    active: 2,
                    ..
                })
            ),
            "expected StaleGeneration"
        );
    };
    stale(i.apply(&[rep(c, 1)]));
    stale(i.apply(&[ins(c, 1, resp(1, 5, "late", u, d))]));
    stale(i.apply(&[tomb(c, 1, p(1, 0))]));
    purge_all(&i);
    assert_eq!(i.stored_entries().unwrap(), 2);
    assert_eq!(ids(&dump(&i, c)), ["e2"]);
    // No staged checkpoint of generation 2: the main slot is cleared by the switch.
    assert_eq!(i.state(c).unwrap().unwrap().checkpoint, None);
    i.apply(&[rep(c, 3)]).unwrap();
    let hits = dump(&i, c);
    assert_eq!((ids(&hits), hits[0].generation), (vec!["e3".to_owned()], 3));
}

/// Dropping a collection hides it entirely and leaves the others alone.
pub fn drop_collection<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let (c, d) = (key(1), key(2));
    let i = new();
    let dt = date(2020, 1, 1, 0, 0, 0);
    i.apply(&[
        rep(c, 1),
        rep(d, 1),
        ins(c, 1, resp(1, 0, "x", "http://a.test/", dt)),
        ins(d, 1, resp(1, 0, "y", "http://a.test/", dt)),
        cp(c, 1, 10),
        cp(c, 2, 20),
    ])
    .unwrap();
    assert_eq!(
        i.state(c)
            .unwrap()
            .unwrap()
            .staged_checkpoint
            .map(|k| k.cdx_offset),
        Some(20)
    );
    i.apply(&[Op::DropCollection { collection: c }]).unwrap();
    assert_eq!(i.state(c).unwrap(), None);
    assert_eq!(i.collections().unwrap(), [d]);
    assert!(i.by_id(None, "x").unwrap().is_none());
    assert!(
        i.by_url_nearest(c, "http://a.test/", None)
            .unwrap()
            .is_none()
    );
    assert!(query(&i, c, &host_any("a.test")).is_empty());
    assert!(dump(&i, c).is_empty());
    assert_eq!(ids(&dump(&i, d)), ["y"]);
    i.apply(&[Op::DropCollection {
        collection: key(77),
    }])
    .unwrap();
    // Both checkpoint slots are gone after a recreate.
    i.apply(&[rep(c, 1)]).unwrap();
    assert_eq!(
        i.state(c).unwrap(),
        Some(CollectionState {
            active_generation: Some(1),
            checkpoint: None,
            staged_checkpoint: None
        })
    );
}

/// A recreated collection never sees rows from before the drop.
pub fn drop_then_recreate_never_resurrects<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let dt = date(2020, 1, 1, 0, 0, 0);
    let u = "http://e.test/u";
    let old = || ins(c, 1, resp(1, 0, "old", u, dt));
    let newer = || ins(c, 1, resp(1, 100, "new", u, dt));
    let check = |i: &I, expected: &[&str]| {
        assert_eq!(ids(&dump(i, c)), expected);
        assert!(i.by_id(None, "old").unwrap().is_none());
        if expected.contains(&"old2") {
            let h = i.by_id(None, "old2").unwrap().unwrap();
            assert_eq!((h.position(), h.entry.raw_size), (p(1, 0), 77));
        }
        let near = nearest_id(i, c, u, None);
        assert_eq!(near.as_deref(), expected.last().copied());
        assert_eq!(ids(&query(i, c, &host_any("e.test"))), expected);
    };
    for variant in 0..3 {
        let i = active(new, c);
        i.apply(&[old()]).unwrap();
        let mut expected = vec!["new"];
        match variant {
            0 => i
                .apply(&[Op::DropCollection { collection: c }, rep(c, 1), newer()])
                .unwrap(),
            1 => {
                i.apply(&[Op::DropCollection { collection: c }]).unwrap();
                i.apply(&[rep(c, 1), newer()]).unwrap();
            }
            _ => {
                i.apply(&[
                    Op::DropCollection { collection: c },
                    rep(c, 1),
                    newer(),
                    ins(c, 1, {
                        let mut e = resp(1, 0, "old2", u, dt);
                        e.raw_size = 77;
                        e
                    }),
                ])
                .unwrap();
                expected = vec!["old2", "new"];
            }
        }
        check(&i, &expected);
        purge_all(&i);
        check(&i, &expected);
        assert_eq!(
            i.stored_entries().unwrap(),
            u64::try_from(expected.len()).unwrap()
        );
    }
}

fn host_case<I: SecondaryIndex>(new: &dyn Fn() -> I, list: &[&str]) -> (I, CollectionKey) {
    let c = key(1);
    let i = active(new, c);
    let ops: Vec<Op<'static>> = list
        .iter()
        .enumerate()
        .map(|(n, u)| {
            ins(
                c,
                1,
                resp(
                    1,
                    u64::try_from(n).unwrap() * 10,
                    &format!("id{n}"),
                    u,
                    date(2020, 1, 1, 0, 0, 0),
                ),
            )
        })
        .collect();
    i.apply(&ops).unwrap();
    (i, c)
}

/// Host patterns, with and without subdomains.
pub fn pattern_host_and_subdomains<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let (i, c) = host_case(
        new,
        &[
            "http://example.com/a",
            "http://www.example.com/b",
            "http://a.b.example.com/c",
            "http://examples.com/d",
            "http://example.org/e",
        ],
    );
    let sub = pat(
        "example.com",
        HostMatch::WithSubdomains,
        PortFilter::Any,
        PathFilter::Any,
    );
    assert_eq!(
        urls(&query(&i, c, &sub)),
        [
            "http://example.com/a",
            "http://a.b.example.com/c",
            "http://www.example.com/b"
        ]
    );
    assert_eq!(
        urls(&query(&i, c, &host_any("example.com"))),
        ["http://example.com/a"]
    );
}

/// Port filters.
pub fn pattern_ports<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let all = [
        "http://h.test/",
        "http://h.test:8080/",
        "https://h.test:8443/",
        "https://h.test/",
    ];
    let (i, c) = host_case(new, &all);
    let q = |pf| {
        sorted_urls(&query(
            &i,
            c,
            &pat("h.test", HostMatch::Exact, pf, PathFilter::Any),
        ))
    };
    assert_eq!(
        q(PortFilter::Default),
        ["http://h.test/", "https://h.test/"]
    );
    assert_eq!(q(PortFilter::Explicit(8080)), ["http://h.test:8080/"]);
    let mut expected: Vec<String> = all.iter().map(|s| (*s).to_owned()).collect();
    expected.sort();
    assert_eq!(q(PortFilter::Any), expected);
}

/// Path filters.
pub fn pattern_paths<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let (i, c) = host_case(
        new,
        &[
            "http://p.test/a",
            "http://p.test/a/b",
            "http://p.test/a/b?x=1",
            "http://p.test/ab",
            "http://p.test/b",
        ],
    );
    let q = |f| {
        sorted_urls(&query(
            &i,
            c,
            &pat("p.test", HostMatch::Exact, PortFilter::Any, f),
        ))
    };
    assert_eq!(q(PathFilter::Exact("/a/b")), ["http://p.test/a/b"]);
    assert_eq!(
        q(PathFilter::Prefix("/a/")),
        ["http://p.test/a/b", "http://p.test/a/b?x=1"]
    );
    assert_eq!(
        q(PathFilter::Prefix("/a")),
        [
            "http://p.test/a",
            "http://p.test/a/b",
            "http://p.test/a/b?x=1",
            "http://p.test/ab"
        ]
    );
    assert_eq!(q(PathFilter::Exact("/a/b?x=1")), ["http://p.test/a/b?x=1"]);
}

/// IP hosts.
pub fn pattern_ip_hosts<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let (i, c) = host_case(new, &["http://127.0.0.1/x", "http://[::1]:8080/y"]);
    let v4 = pat(
        "127.0.0.1",
        HostMatch::Exact,
        PortFilter::Any,
        PathFilter::Any,
    );
    assert_eq!(urls(&query(&i, c, &v4)), ["http://127.0.0.1/x"]);
    let v6 = pat(
        "::1",
        HostMatch::Exact,
        PortFilter::Explicit(8080),
        PathFilter::Any,
    );
    assert_eq!(urls(&query(&i, c, &v6)), ["http://[::1]:8080/y"]);
}

/// Pattern results are ordered by (key, date, position) and only contain visible entries.
pub fn pattern_order_and_visibility<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let (c, d) = (key(1), key(2));
    let i = new();
    let u = "http://order.test/p";
    i.apply(&[
        rep(c, 1),
        rep(d, 1),
        ins(c, 1, resp(1, 0, "d2020", u, date(2020, 1, 1, 0, 0, 0))),
        ins(c, 1, resp(1, 10, "d2010", u, date(2010, 1, 1, 0, 0, 0))),
        ins(c, 1, resp(1, 20, "d2015", u, date(2015, 1, 1, 0, 0, 0))),
        ins(
            c,
            1,
            add(1, 30, "req", Some(u), date(2012, 1, 1, 0, 0, 0), "request"),
        ),
        ins(c, 1, resp(1, 40, "gone", u, date(2011, 1, 1, 0, 0, 0))),
        tomb(c, 1, p(1, 40)),
        ins(c, 2, resp(2, 0, "staged", u, date(2013, 1, 1, 0, 0, 0))),
        ins(d, 1, resp(1, 0, "other", u, date(2014, 1, 1, 0, 0, 0))),
        ins(
            c,
            1,
            resp(1, 50, "raw", "urn:order:1", date(2014, 1, 1, 0, 0, 0)),
        ),
        ins(
            c,
            1,
            add(1, 60, "nourl", None, date(2014, 1, 1, 0, 0, 0), "response"),
        ),
    ])
    .unwrap();
    let hits = query(&i, c, &host_any("order.test"));
    assert_eq!(ids(&hits), ["d2010", "req", "d2015", "d2020"]);
}

fn pages<I: SecondaryIndex>(
    i: &I,
    c: CollectionKey,
    pattern: &UrlPattern,
    limit: usize,
) -> Vec<crate::Page> {
    let mut out: Vec<crate::Page> = Vec::new();
    let mut q = PatternQuery::new(limit);
    loop {
        let page = i.by_pattern(c, pattern, &q).unwrap();
        let next = page.next.clone();
        out.push(page);
        match next {
            Some(cur) => q = PatternQuery::new(limit).after(cur),
            None => return out,
        }
    }
}

/// Pagination covers everything exactly once, with an exact `next`.
pub fn pattern_pagination_complete<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let i = active(new, c);
    let mut ops: Vec<Op<'static>> = Vec::new();
    let mut n = 0u64;
    for h in 0..10 {
        for day in 1..=25u8 {
            ops.push(ins(
                c,
                1,
                resp(
                    1,
                    n * 10,
                    &format!("m{n}"),
                    &format!("http://h{h}.match.test/p"),
                    date(2020, 1, day, 0, 0, 0),
                ),
            ));
            n += 1;
        }
    }
    for k in 0..40 {
        ops.push(ins(
            c,
            1,
            resp(
                1,
                n * 10,
                &format!("o{k}"),
                &format!("http://x{k}.other.test/p"),
                date(2020, 1, 1, 0, 0, 0),
            ),
        ));
        n += 1;
    }
    i.apply(&ops).unwrap();
    let pattern = pat(
        "match.test",
        HostMatch::WithSubdomains,
        PortFilter::Any,
        PathFilter::Any,
    );
    let all = query(&i, c, &pattern);
    assert_eq!(all.len(), 250);
    let ps = pages(&i, c, &pattern, 7);
    let paged: Vec<Hit> = ps.iter().flat_map(|pg| pg.hits.clone()).collect();
    assert_eq!(paged, all);
    let (last, rest) = ps.split_last().unwrap();
    assert!(last.next.is_none());
    for pg in rest {
        assert_eq!(pg.hits.len(), 7);
        let next = pg.next.clone().unwrap();
        let token = Cursor::from_token(&next.to_token()).unwrap();
        assert_eq!(token, next);
    }
    // A token survives the round trip and gives the same following page.
    let first = i.by_pattern(c, &pattern, &PatternQuery::new(7)).unwrap();
    let tok = Cursor::from_token(&first.next.clone().unwrap().to_token()).unwrap();
    let second = i
        .by_pattern(c, &pattern, &PatternQuery::new(7).after(tok))
        .unwrap();
    assert_eq!(second, ps[1]);

    // Exactly two full pages: the second has no next.
    let j = active(new, c);
    let ops: Vec<Op<'static>> = (0..14u64)
        .map(|k| {
            ins(
                c,
                1,
                resp(
                    1,
                    k * 10,
                    &format!("e{k}"),
                    "http://h.match.test/p",
                    date(2020, 1, 1, 0, 0, 0),
                ),
            )
        })
        .collect();
    j.apply(&ops).unwrap();
    let ps = pages(&j, c, &pattern, 7);
    assert_eq!(ps.len(), 2);
    assert_eq!(ps[1].hits.len(), 7);
    assert!(ps[0].next.is_some() && ps[1].next.is_none());
}

/// Cursors give "strictly after" semantics under concurrent changes.
pub fn pattern_pagination_stable<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let i = active(new, c);
    let u = "http://s.test/p";
    let ops: Vec<Op<'static>> = (11..=30u8)
        .map(|day| {
            ins(
                c,
                1,
                resp(
                    1,
                    u64::from(day) * 10,
                    &format!("d{day}"),
                    u,
                    date(2020, 1, day, 0, 0, 0),
                ),
            )
        })
        .collect();
    i.apply(&ops).unwrap();
    let pattern = host_any("s.test");
    let page1 = i.by_pattern(c, &pattern, &PatternQuery::new(5)).unwrap();
    assert_eq!(ids(&page1.hits), ["d11", "d12", "d13", "d14", "d15"]);
    i.apply(&[
        ins(c, 1, resp(2, 0, "before", u, date(2020, 1, 1, 0, 0, 0))),
        ins(c, 1, resp(2, 10, "after", u, date(2020, 1, 31, 0, 0, 0))),
        tomb(c, 1, p(1, 200)),
    ])
    .unwrap();
    let mut rest: Vec<String> = Vec::new();
    let mut q = PatternQuery::new(5).after(page1.next.clone().unwrap());
    loop {
        let pg = i.by_pattern(c, &pattern, &q).unwrap();
        rest.extend(ids(&pg.hits));
        match pg.next {
            Some(cur) => q = PatternQuery::new(5).after(cur),
            None => break,
        }
    }
    let mut expected: Vec<String> = (16..=30u8)
        .filter(|d| *d != 20)
        .map(|d| format!("d{d}"))
        .collect();
    expected.push("after".to_owned());
    assert_eq!(rest, expected);
}

/// `limit` and `max_entries` validation.
pub fn limits_validated<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let i = active(new, c);
    let pattern = host_any("a.test");
    for limit in [0, MAX_LIMIT + 1] {
        assert!(matches!(
            i.by_pattern(c, &pattern, &PatternQuery::new(limit)),
            Err(Error::InvalidLimit { .. })
        ));
    }
    assert!(matches!(
        i.list(c, None, 0),
        Err(Error::InvalidLimit { .. })
    ));
    assert!(matches!(
        i.list(c, None, MAX_LIMIT + 1),
        Err(Error::InvalidLimit { .. })
    ));
    assert!(matches!(i.purge(0), Err(Error::InvalidLimit { .. })));
    assert!(
        i.by_pattern(c, &pattern, &PatternQuery::new(MAX_LIMIT))
            .is_ok()
    );
    assert!(i.list(c, None, MAX_LIMIT).is_ok());
}

/// `list` is ordered by position and paged by `after`.
pub fn list_order_and_paging<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let i = active(new, c);
    let d = date(2020, 1, 1, 0, 0, 0);
    let u = "http://a.test/";
    i.apply(&[
        ins(c, 1, resp(2, 0, "p20", u, d)),
        ins(c, 1, resp(1, 50, "p150", u, d)),
        ins(c, 1, resp(1, 0, "p10", u, d)),
        ins(c, 1, resp(3, 5, "p35", u, d)),
        ins(c, 2, resp(1, 1, "staged", u, d)),
    ])
    .unwrap();
    let all = i.list(c, None, 100).unwrap();
    assert_eq!(ids(&all), ["p10", "p150", "p20", "p35"]);
    assert_eq!(
        ids(&i.list(c, Some(p(1, 50)), 100).unwrap()),
        ["p20", "p35"]
    );
    assert_eq!(ids(&i.list(c, Some(p(1, 49)), 1).unwrap()), ["p150"]);
    assert_eq!(ids(&dump(&i, c)), ["p10", "p150", "p20", "p35"]);
}

/// Checkpoints: last write wins, creates the state, cleared by drop.
pub fn checkpoint_basic<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let (c, d) = (key(1), key(2));
    let i = new();
    let got = |i: &I, c| i.state(c).unwrap().and_then(|s| s.checkpoint);
    let ck = |generation, cdx_offset| {
        Some(Checkpoint {
            generation,
            cdx_offset,
        })
    };
    i.apply(&[cp(c, 1, 10)]).unwrap();
    assert_eq!(got(&i, c), ck(1, 10));
    i.apply(&[cp(c, 1, 5)]).unwrap();
    assert_eq!(got(&i, c), ck(1, 5));
    assert_eq!(i.collections().unwrap(), [c]);
    i.apply(&[cp(c, 1, 20), cp(d, 2, 30)]).unwrap();
    assert_eq!((got(&i, c), got(&i, d)), (ck(1, 20), ck(2, 30)));
    i.apply(&[Op::DropCollection { collection: c }]).unwrap();
    assert_eq!(i.state(c).unwrap(), None);
}

/// A failing batch changes nothing.
pub fn batch_atomic_on_error<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let (c, d) = (key(1), key(2));
    let dt = date(2020, 1, 1, 0, 0, 0);
    let u = "http://a.test/";
    let mut bad_size = resp(1, 20, "bad", u, dt);
    bad_size.raw_size = 0;
    let mut bad_kind = resp(1, 20, "bad", u, dt);
    bad_kind.kind = Kind::Tombstone;
    let a = || ins(c, 1, resp(1, 0, "a", u, dt));
    let setup = || {
        let i = active(new, c);
        i.apply(&[ins(c, 1, resp(1, 100, "base", u, dt)), cp(c, 1, 1)])
            .unwrap();
        i
    };
    let snap = |i: &I| {
        (
            dump(i, c),
            i.state(c).unwrap(),
            i.state(d).unwrap(),
            i.stored_entries().unwrap(),
        )
    };
    type Case = (Vec<Op<'static>>, fn(&Error) -> bool);
    let cases: Vec<Case> = vec![
        (vec![a(), cp(c, 1, 99), ins(c, 1, bad_size.clone())], |e| {
            matches!(e, Error::InvalidEntry { op: 2, .. })
        }),
        (vec![a(), cp(c, 1, 99), ins(c, 1, bad_kind)], |e| {
            matches!(e, Error::NotAnAdd { op: 2 })
        }),
        (
            vec![a(), rep(c, 5), ins(c, 3, resp(1, 30, "late", u, dt))],
            |e| {
                matches!(
                    e,
                    Error::StaleGeneration {
                        op: 2,
                        generation: 3,
                        active: 5,
                        ..
                    }
                )
            },
        ),
        (vec![a(), rep(c, 5), cp(c, 3, 1)], |e| {
            matches!(
                e,
                Error::StaleGeneration {
                    op: 2,
                    generation: 3,
                    active: 5,
                    ..
                }
            )
        }),
        (vec![a(), cp(c, 1, 99), ins(d, 1, bad_size)], |e| {
            matches!(e, Error::InvalidEntry { op: 2, .. })
        }),
    ];
    for (ops, expected) in cases {
        let i = setup();
        let before = snap(&i);
        let err = i.apply(&ops).unwrap_err();
        assert!(expected(&err), "{err:?}");
        assert_eq!(snap(&i), before);
    }
}

/// Ops of a batch are applied in order.
pub fn ops_apply_in_order<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let (c, d) = (key(1), key(2));
    let dt = date(2020, 1, 1, 0, 0, 0);
    let u = "http://a.test/";
    let i = new();
    i.apply(&[rep(c, 1), rep(d, 1), ins(d, 1, resp(1, 0, "w", u, dt))])
        .unwrap();
    i.apply(&[
        ins(c, 1, resp(1, 0, "x", u, dt)),
        tomb(c, 1, p(1, 0)),
        ins(c, 1, resp(1, 10, "y", u, dt)),
        Op::DropCollection { collection: d },
        rep(d, 1),
        ins(d, 1, resp(1, 20, "z", u, dt)),
    ])
    .unwrap();
    assert_eq!(ids(&dump(&i, c)), ["y"]);
    assert_eq!(ids(&dump(&i, d)), ["z"]);
}

fn random_batches(rng: &mut Rng, c: CollectionKey, count: usize) -> Vec<Vec<Op<'static>>> {
    let mut live: Vec<Entry<'static>> = Vec::new();
    let mut n = 0u64;
    let mut out = Vec::new();
    for k in 0..count {
        let mut batch = Vec::new();
        for _ in 0..(1 + rng.below(6)) {
            if !live.is_empty() && rng.below(4) == 0 {
                let e = live.swap_remove(rng.below(live.len()));
                batch.push(tomb(c, 1, p(e.segment, e.offset)));
            } else {
                let e = random_entry(rng, n, 1);
                n += 1;
                live.push(e.clone());
                batch.push(ins(c, 1, e));
            }
        }
        batch.push(cp(c, 1, u64::try_from(k).unwrap()));
        out.push(batch);
    }
    out
}

fn random_entry(rng: &mut Rng, n: u64, segment_base: u32) -> Entry<'static> {
    let id = format!("urn:uuid:{:04}", rng.below(50));
    let u = rng.below(30);
    let url = match u {
        0..10 => Some(format!("http://h{u}.test/p")),
        10..20 => Some(format!("https://h{}.test/p?x={u}", u - 10)),
        20..25 => Some(format!("urn:x:{u}")),
        _ => None,
    };
    let t = ["response", "request", "metadata"][rng.below(3)];
    let d = date(
        u16::try_from(2000 + rng.below(31)).unwrap(),
        u8::try_from(1 + rng.below(12)).unwrap(),
        u8::try_from(1 + rng.below(28)).unwrap(),
        0,
        0,
        0,
    );
    add(
        segment_base + u32::try_from(n / 100).unwrap(),
        (n % 100) * 100,
        &id,
        url.as_deref(),
        d,
        t,
    )
}

/// Replaying batches from an older checkpoint converges.
pub fn idempotent_replay<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let i = active(new, c);
    let batches = random_batches(&mut Rng(0x9E37_79B9_7F4A_7C15), c, 30);
    for b in &batches {
        i.apply(b).unwrap();
    }
    let s = (dump(&i, c), i.state(c).unwrap());
    for b in &batches[10..] {
        i.apply(b).unwrap();
    }
    assert_eq!((dump(&i, c), i.state(c).unwrap()), s);
    for b in &batches {
        i.apply(b).unwrap();
    }
    assert_eq!((dump(&i, c), i.state(c).unwrap()), s);
}

/// I8: an index rebuilt from the live set equals the incrementally built one.
pub fn rebuilt_equals_incremental<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let a = active(new, c);
    let mut model: BTreeMap<Position, Entry<'static>> = BTreeMap::new();
    let mut generation = 1u32;
    let mut counter = 0u64;
    let mut ops_done = 0usize;
    let mut batch_no = 0u64;
    let mut rebuilt = false;
    while ops_done < 2000 {
        if ops_done >= 1000 && !rebuilt {
            rebuilt = true;
            // Stage generation 2: live entries copied in position order to fresh positions.
            let copies: Vec<Entry<'static>> = model
                .values()
                .map(|e| {
                    let mut e = e.clone();
                    let pos = counter;
                    counter += 1;
                    e.segment = 500 + u32::try_from(pos / 100).unwrap();
                    e.offset = (pos % 100) * 100;
                    e
                })
                .collect();
            let before = dump(&a, c);
            // An abandoned staging attempt: bogus entries (one at a position the real staging
            // reuses, with another raw_size), then discarded.
            let mut bogus: Vec<Op<'static>> = (0..5u64)
                .map(|k| {
                    ins(
                        c,
                        2,
                        resp(
                            900,
                            k * 100,
                            &format!("bogus{k}"),
                            "http://bogus.test/",
                            date(2001, 1, 1, 0, 0, 0),
                        ),
                    )
                })
                .collect();
            if let Some(first) = copies.first() {
                let mut e = first.clone();
                e.raw_size += 1;
                bogus.push(ins(c, 2, e));
            }
            bogus.push(cp(c, 2, 1));
            a.apply(&bogus).unwrap();
            a.apply(&[Op::DiscardStaged {
                collection: c,
                generation: 2,
            }])
            .unwrap();
            for chunk in copies.chunks(copies.len() / 3 + 1) {
                let mut b: Vec<Op<'static>> = chunk.iter().map(|e| ins(c, 2, e.clone())).collect();
                batch_no += 1;
                b.push(cp(c, 2, batch_no));
                a.apply(&b).unwrap();
            }
            assert_eq!(dump(&a, c), before, "staged entries must stay invisible");
            batch_no += 1;
            a.apply(&[rep(c, 2), cp(c, 2, batch_no)]).unwrap();
            generation = 2;
            model = copies
                .into_iter()
                .map(|e| (p(e.segment, e.offset), e))
                .collect();
            continue;
        }
        let mut batch: Vec<Op<'static>> = Vec::new();
        for _ in 0..(1 + rng.below(20)) {
            if !model.is_empty() && rng.below(4) == 0 {
                let pos = *model.keys().nth(rng.below(model.len())).unwrap();
                model.remove(&pos);
                batch.push(tomb(c, generation, pos));
            } else {
                let e = random_entry(&mut rng, counter, 1);
                counter += 1;
                model.insert(p(e.segment, e.offset), e.clone());
                batch.push(ins(c, generation, e));
            }
            ops_done += 1;
        }
        batch_no += 1;
        batch.push(cp(c, generation, batch_no));
        a.apply(&batch).unwrap();
    }

    let b = new();
    b.apply(&[rep(c, 2)]).unwrap();
    let live: Vec<Entry<'static>> = model.values().cloned().collect();
    for chunk in live.chunks(100) {
        let ops: Vec<Op<'static>> = chunk.iter().map(|e| ins(c, 2, e.clone())).collect();
        b.apply(&ops).unwrap();
    }
    b.apply(&[cp(c, 2, batch_no)]).unwrap();

    let (da, db) = (dump(&a, c), dump(&b, c));
    assert_eq!(da, db);
    assert_eq!(
        da.iter().map(|h| &h.entry).collect::<Vec<_>>(),
        live.iter().collect::<Vec<_>>()
    );
    assert!(da.iter().all(|h| h.generation == 2 && h.collection == c));
    assert_eq!(a.state(c).unwrap(), b.state(c).unwrap());

    for n in 0..50 {
        let id = format!("urn:uuid:{n:04}");
        assert_eq!(
            a.by_id(None, &id).unwrap(),
            b.by_id(None, &id).unwrap(),
            "{id}"
        );
    }
    let dates = [
        Some(date(2000, 1, 1, 0, 0, 0)),
        Some(date(2008, 6, 15, 0, 0, 0)),
        Some(date(2015, 3, 3, 0, 0, 0)),
        Some(date(2030, 12, 31, 0, 0, 0)),
        None,
    ];
    for u in 0..30 {
        let url = match u {
            0..10 => format!("http://h{u}.test/p"),
            10..20 => format!("https://h{}.test/p?x={u}", u - 10),
            20..25 => format!("urn:x:{u}"),
            _ => "http://none.test/".to_owned(),
        };
        for d in dates {
            assert_eq!(
                a.by_url_nearest(c, &url, d).unwrap(),
                b.by_url_nearest(c, &url, d).unwrap(),
                "{url} {d:?}"
            );
        }
    }
    let patterns = [
        host_any("h3.test"),
        pat(
            "test",
            HostMatch::WithSubdomains,
            PortFilter::Any,
            PathFilter::Prefix("/p"),
        ),
        pat(
            "test",
            HostMatch::WithSubdomains,
            PortFilter::Default,
            PathFilter::Any,
        ),
        pat(
            "h2.test",
            HostMatch::Exact,
            PortFilter::Any,
            PathFilter::Exact("/p?x=12"),
        ),
    ];
    let mut total = 0;
    for pattern in &patterns {
        let (pa, pb) = (pages(&a, c, pattern, 9), pages(&b, c, pattern, 9));
        assert_eq!(pa, pb);
        total += pa.iter().map(|pg| pg.hits.len()).sum::<usize>();
    }
    assert!(total > 0);
}

/// `DiscardStaged` clears an abandoned staging attempt; the generation can be restaged.
pub fn discard_staged_then_restage<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let i = active(new, c);
    let d = date(2020, 1, 1, 0, 0, 0);
    let u = "http://a.test/";
    i.apply(&[ins(c, 1, resp(1, 500, "a", u, d))]).unwrap();
    i.apply(&[
        ins(c, 2, resp(1, 0, "p1", u, d)),
        ins(c, 2, resp(1, 10, "p2", u, d)),
        cp(c, 2, 40),
    ])
    .unwrap();
    assert!(i.state(c).unwrap().unwrap().staged_checkpoint.is_some());
    i.apply(&[Op::DiscardStaged {
        collection: c,
        generation: 2,
    }])
    .unwrap();
    assert_eq!(i.state(c).unwrap().unwrap().staged_checkpoint, None);
    let mut p1 = resp(1, 0, "p1", u, d);
    p1.raw_size = 20;
    i.apply(&[ins(c, 2, p1.clone())]).unwrap();
    i.apply(&[rep(c, 2)]).unwrap();
    for purged in [false, true] {
        if purged {
            purge_all(&i);
            assert_eq!(i.stored_entries().unwrap(), 1);
        }
        let hits = dump(&i, c);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry, p1);
        assert!(i.by_id(None, "p2").unwrap().is_none());
    }
    let not_staged = |r: Result<(), Error>| {
        assert!(
            matches!(r, Err(Error::NotStaged { op: 0, .. })),
            "expected NotStaged"
        );
    };
    not_staged(i.apply(&[Op::DiscardStaged {
        collection: c,
        generation: 2,
    }]));
    not_staged(i.apply(&[Op::DiscardStaged {
        collection: c,
        generation: 1,
    }]));
    let unknown = key(9);
    i.apply(&[Op::DiscardStaged {
        collection: unknown,
        generation: 4,
    }])
    .unwrap();
    assert_eq!(i.state(unknown).unwrap(), None);
    i.apply(&[
        Op::DiscardStaged {
            collection: c,
            generation: 3,
        },
        ins(c, 3, resp(2, 0, "x", u, d)),
    ])
    .unwrap();
    assert_eq!(ids(&dump(&i, c)), ["p1"]);
    i.apply(&[rep(c, 3)]).unwrap();
    assert_eq!(ids(&dump(&i, c)), ["x"]);
}

/// Active and staged checkpoint slots are routed and carried over as specified.
pub fn staged_checkpoint_separate<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let i = active(new, c);
    let ck = |generation, cdx_offset| {
        Some(Checkpoint {
            generation,
            cdx_offset,
        })
    };
    let slots = |i: &I| {
        let s = i.state(c).unwrap().unwrap();
        (s.checkpoint, s.staged_checkpoint)
    };
    i.apply(&[cp(c, 1, 100)]).unwrap();
    i.apply(&[cp(c, 2, 50)]).unwrap();
    assert_eq!(slots(&i), (ck(1, 100), ck(2, 50)));
    i.apply(&[cp(c, 1, 120)]).unwrap();
    assert_eq!(slots(&i), (ck(1, 120), ck(2, 50)));
    i.apply(&[cp(c, 3, 7)]).unwrap();
    assert_eq!(slots(&i), (ck(1, 120), ck(3, 7)));
    i.apply(&[cp(c, 2, 50)]).unwrap();
    i.apply(&[rep(c, 2)]).unwrap();
    assert_eq!(slots(&i), (ck(2, 50), None));
    // A staged checkpoint of a newer generation survives a real activation.
    let j = active(new, c);
    j.apply(&[cp(c, 1, 100), cp(c, 3, 7)]).unwrap();
    j.apply(&[rep(c, 2)]).unwrap();
    assert_eq!(slots(&j), (None, ck(3, 7)));
    i.apply(&[cp(c, 3, 7)]).unwrap();
    i.apply(&[rep(c, 2)]).unwrap();
    assert_eq!(slots(&i), (ck(2, 50), ck(3, 7)));
    assert!(matches!(
        i.apply(&[cp(c, 1, 5)]),
        Err(Error::StaleGeneration {
            op: 0,
            generation: 1,
            active: 2,
            ..
        })
    ));
    i.apply(&[Op::DropCollection { collection: c }]).unwrap();
    i.apply(&[rep(c, 1)]).unwrap();
    assert_eq!(slots(&i), (None, None));
}

/// Both `by_id` levels at once: per-collection commit order, then date across collections.
pub fn by_id_two_levels<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let (c, d) = (key(1), key(2));
    let i = new();
    let u = "http://a.test/";
    i.apply(&[
        rep(c, 1),
        rep(d, 1),
        // In C the later commit (1,10) has the older date.
        ins(c, 1, resp(1, 0, "x", u, date(2020, 1, 1, 0, 0, 0))),
        ins(c, 1, resp(1, 10, "x", u, date(2010, 1, 1, 0, 0, 0))),
        ins(d, 1, resp(1, 0, "x", u, date(2015, 1, 1, 0, 0, 0))),
    ])
    .unwrap();
    let in_c = i.by_id(Some(c), "x").unwrap().unwrap();
    assert_eq!(in_c.position(), p(1, 10));
    let cands = |scope: &[(CollectionKey, i128, Position)]| {
        crate::rank::pick_by_id(scope.iter().map(|&(collection, date_nanos, position)| {
            crate::rank::IdCandidate {
                collection,
                date_nanos,
                position,
            }
        }))
        .unwrap()
    };
    let n = |y| date(y, 1, 1, 0, 0, 0).unix_nanos();
    let expected = cands(&[
        (c, n(2020), p(1, 0)),
        (c, n(2010), p(1, 10)),
        (d, n(2015), p(1, 0)),
    ]);
    assert_eq!(expected.collection, d);
    let global = i.by_id(None, "x").unwrap().unwrap();
    assert_eq!(
        (global.collection, global.position()),
        (expected.collection, expected.position)
    );
    assert_eq!(i.by_id(Some(d), "x").unwrap().unwrap().collection, d);
}

/// Readers never observe part of a batch.
pub fn concurrent_readers_see_whole_batches<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let i = new();
    let done = AtomicBool::new(false);
    let batch_of = |n: u32| -> Vec<Op<'static>> {
        let mut ops: Vec<Op<'static>> = Vec::new();
        if n == 0 {
            ops.push(rep(c, 1));
        } else {
            ops.extend((0..5u64).map(|j| tomb(c, 1, p(n - 1, j * 10))));
        }
        ops.extend((0..5u64).map(|j| {
            ins(
                c,
                1,
                resp(
                    n,
                    j * 10,
                    &format!("b{n}-{j}"),
                    "http://conc.test/x",
                    date(2020, 1, 1, 0, 0, 0),
                ),
            )
        }));
        ops.push(cp(c, 1, u64::from(n)));
        ops
    };
    let start = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                let pattern = host_any("conc.test");
                let mut iterations = 0;
                while !done.load(Ordering::SeqCst) || iterations < 50 {
                    assert!(start.elapsed() < TIMEOUT, "timeout");
                    iterations += 1;
                    let cp_seen = i.state(c).unwrap().and_then(|st| st.checkpoint);
                    let by_pat = query(&i, c, &pattern);
                    let listed = i.list(c, None, MAX_LIMIT).unwrap();
                    for hits in [&by_pat, &listed] {
                        if cp_seen.is_none() && hits.is_empty() {
                            // Only allowed before batch 0 is committed.
                            continue;
                        }
                        assert_eq!(hits.len(), 5, "partial or missing batch");
                        let n = hits[0].entry.segment;
                        assert!(hits.iter().all(|h| h.entry.segment == n));
                        if let Some(ck) = cp_seen {
                            assert!(u64::from(n) >= ck.cdx_offset, "went back in time");
                        }
                        i.by_id(None, &format!("b{n}-0")).unwrap();
                    }
                }
            });
        }
        let _guard = Done(&done);
        for n in 0..200u32 {
            i.apply(&batch_of(n)).unwrap();
        }
    });
    let hits = dump(&i, c);
    assert_eq!(hits.len(), 5);
    assert!(hits.iter().all(|h| h.entry.segment == 199));
}

/// Concurrent writers on distinct collections.
pub fn concurrent_writers<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let i = new();
    let finished = AtomicUsize::new(0);
    let start = Instant::now();
    std::thread::scope(|s| {
        for t in 0..4u128 {
            let (i, finished) = (&i, &finished);
            s.spawn(move || {
                let _bump = Bump(finished);
                let c = key(10 + t);
                i.apply(&[rep(c, 1)]).unwrap();
                for k in 0..50u32 {
                    let mut ops: Vec<Op<'static>> = (0..10u64)
                        .map(|j| {
                            ins(
                                c,
                                1,
                                resp(
                                    k,
                                    j * 10,
                                    &format!("w{t}-{k}-{j}"),
                                    "http://w.test/",
                                    date(2020, 1, 1, 0, 0, 0),
                                ),
                            )
                        })
                        .collect();
                    ops.push(cp(c, 1, u64::from(k)));
                    i.apply(&ops).unwrap();
                }
            });
        }
        for _ in 0..2 {
            s.spawn(|| {
                while finished.load(Ordering::SeqCst) < 4 {
                    assert!(start.elapsed() < TIMEOUT, "timeout");
                    for c in i.collections().unwrap() {
                        i.list(c, None, 100).unwrap();
                    }
                }
            });
        }
    });
    for t in 0..4u128 {
        let c = key(10 + t);
        assert_eq!(dump(&i, c).len(), 500);
        assert_eq!(
            i.state(c).unwrap().unwrap().checkpoint,
            Some(Checkpoint {
                generation: 1,
                cdx_offset: 49
            })
        );
    }
}

struct Bump<'a>(&'a AtomicUsize);
impl Drop for Bump<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Invalid entries are rejected with the right error.
pub fn insert_validation<I: SecondaryIndex>(new: &dyn Fn() -> I) {
    let c = key(1);
    let i = active(new, c);
    let dt = date(2020, 1, 1, 0, 0, 0);
    let ok = resp(1, 0, "x", "http://a.test/", dt);
    let mut empty_id = ok.clone();
    empty_id.record_id = Cow::Borrowed("");
    let mut empty_url = ok.clone();
    empty_url.url = Some(Cow::Borrowed(""));
    let mut zero = ok.clone();
    zero.raw_size = 0;
    for e in [empty_id, empty_url, zero] {
        assert!(matches!(
            i.apply(&[ins(c, 1, e)]),
            Err(Error::InvalidEntry { op: 0, .. })
        ));
    }
    let mut t = ok;
    t.kind = Kind::Tombstone;
    assert!(matches!(
        i.apply(&[ins(c, 1, t)]),
        Err(Error::NotAnAdd { op: 0 })
    ));
    assert_eq!(i.stored_entries().unwrap(), 0);
}

/// Instantiates every conformance case as a `#[test]` for the index built by `$new`
/// (a `Fn() -> impl SecondaryIndex`).
#[macro_export]
macro_rules! conformance_tests {
    ($new:expr) => {
        $crate::conformance_tests!(@each $new;
            empty_index, insert_invisible_until_activation, by_id_scope,
            by_id_most_recent_commit, by_id_tombstone_falls_back,
            same_record_id_distinct_copies, nearest_exact_match, nearest_before_after,
            nearest_tie_prefers_later, nearest_same_date_prefers_latest_commit,
            nearest_without_date_is_latest, nearest_excludes_request, nearest_url_normalised,
            nearest_raw_urls_exact, nearest_scoped_to_collection, tombstone_hides_everywhere,
            insert_duplicate_identity_is_noop, replace_generation_switches_atomically,
            replace_generation_rules, drop_collection, drop_then_recreate_never_resurrects,
            pattern_host_and_subdomains, pattern_ports, pattern_paths, pattern_ip_hosts,
            pattern_order_and_visibility, pattern_pagination_complete,
            pattern_pagination_stable, limits_validated, list_order_and_paging,
            checkpoint_basic, batch_atomic_on_error, ops_apply_in_order, idempotent_replay,
            rebuilt_equals_incremental, discard_staged_then_restage, staged_checkpoint_separate,
            by_id_two_levels, concurrent_readers_see_whole_batches,
            concurrent_writers, insert_validation
        );
    };
    (@each $new:expr; $($name:ident),* $(,)?) => {
        $( #[test] fn $name() { $crate::conformance::$name(&$new) } )*
    };
}
