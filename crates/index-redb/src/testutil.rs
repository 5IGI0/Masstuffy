//! Test helpers: temporary directory, index wrapper, snapshots and the consistency checker.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use masstuffy_index::conformance::dump;
use masstuffy_index::{
    CollectionKey, CollectionState, Error, Hit, Op, Page, PatternQuery, Position, SecondaryIndex,
    UrlPattern, WarcDate,
};
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata};

use crate::codec::{CollRecord, parse_by_id_key, parse_slot_value};
use crate::schema::{
    BY_ID, COLLECTIONS, DEAD_SLOTS, ENTRIES, KEY_NEXT_SLOT, KEY_STORED, META, SLOTS, URLS,
};
use crate::{Options, RedbIndex};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A fresh temporary directory removed on drop.
pub(crate) struct TestDir(PathBuf);

impl TestDir {
    pub(crate) fn new() -> TestDir {
        let p = std::env::temp_dir().join(format!(
            "masstuffy-index-redb-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        TestDir(p)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn file(&self) -> PathBuf {
        self.0.join("index.redb")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// An index in a temporary directory. Field order matters: `index` drops first.
pub(crate) struct TestIndex {
    pub(crate) index: RedbIndex,
    pub(crate) dir: TestDir,
}

pub(crate) fn fresh() -> TestIndex {
    with(&Options::default())
}

pub(crate) fn with(o: &Options) -> TestIndex {
    let dir = TestDir::new();
    let index = RedbIndex::open(dir.file(), o).unwrap();
    TestIndex { index, dir }
}

impl TestIndex {
    /// Drops the index and reopens the same file.
    pub(crate) fn reopen(self, o: &Options) -> TestIndex {
        let TestIndex { index, dir } = self;
        drop(index);
        let index = RedbIndex::open(dir.file(), o).unwrap();
        TestIndex { index, dir }
    }
}

impl SecondaryIndex for TestIndex {
    fn apply(&self, ops: &[Op<'_>]) -> Result<(), Error> {
        self.index.apply(ops)
    }
    fn state(&self, c: CollectionKey) -> Result<Option<CollectionState>, Error> {
        self.index.state(c)
    }
    fn collections(&self) -> Result<Vec<CollectionKey>, Error> {
        self.index.collections()
    }
    fn by_id(&self, scope: Option<CollectionKey>, rid: &str) -> Result<Option<Hit>, Error> {
        self.index.by_id(scope, rid)
    }
    fn by_url_nearest(
        &self,
        c: CollectionKey,
        url: &str,
        date: Option<WarcDate>,
    ) -> Result<Option<Hit>, Error> {
        self.index.by_url_nearest(c, url, date)
    }
    fn by_pattern(
        &self,
        c: CollectionKey,
        p: &UrlPattern,
        q: &PatternQuery,
    ) -> Result<Page, Error> {
        self.index.by_pattern(c, p, q)
    }
    fn list(
        &self,
        c: CollectionKey,
        after: Option<Position>,
        limit: usize,
    ) -> Result<Vec<Hit>, Error> {
        self.index.list(c, after, limit)
    }
    fn purge(&self, max: usize) -> Result<usize, Error> {
        self.index.purge(max)
    }
    fn stored_entries(&self) -> Result<u64, Error> {
        self.index.stored_entries()
    }
}

/// Observable state of an index.
#[derive(Debug, PartialEq)]
pub(crate) struct Snapshot {
    collections: Vec<CollectionKey>,
    states: Vec<Option<CollectionState>>,
    dumps: Vec<Vec<Hit>>,
    stored: u64,
}

pub(crate) fn snapshot(index: &dyn SecondaryIndex, cs: &[CollectionKey]) -> Snapshot {
    Snapshot {
        collections: index.collections().unwrap(),
        states: cs.iter().map(|c| index.state(*c).unwrap()).collect(),
        dumps: cs.iter().map(|c| dump(index, *c)).collect(),
        stored: index.stored_entries().unwrap(),
    }
}

/// Row counts of every table.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub(crate) dead: u64,
    pub(crate) slots: u64,
    pub(crate) entries: u64,
    pub(crate) by_id: u64,
    pub(crate) urls: u64,
    pub(crate) live_gens: u64,
}

pub(crate) fn counts(index: &RedbIndex) -> Counts {
    let rt = index.db().begin_read().unwrap();
    let mut live_gens = 0u64;
    for row in rt.open_table(COLLECTIONS).unwrap().iter().unwrap() {
        let (_, v) = row.unwrap();
        live_gens += u64::try_from(CollRecord::decode(v.value()).unwrap().gens.len()).unwrap();
    }
    Counts {
        dead: rt.open_table(DEAD_SLOTS).unwrap().len().unwrap(),
        slots: rt.open_table(SLOTS).unwrap().len().unwrap(),
        entries: rt.open_table(ENTRIES).unwrap().len().unwrap(),
        by_id: rt.open_table(BY_ID).unwrap().len().unwrap(),
        urls: rt.open_table(URLS).unwrap().len().unwrap(),
        live_gens,
    }
}

/// `meta.next_slot`.
pub(crate) fn next_slot(index: &RedbIndex) -> u64 {
    let rt = index.db().begin_read().unwrap();
    let meta = rt.open_table(META).unwrap();
    // Bound to a local: the guard borrows `meta`, which must outlive it.
    #[allow(clippy::let_and_return)]
    let v = meta.get(KEY_NEXT_SLOT).unwrap().unwrap().value();
    v
}

/// Checks invariants I-R1..I-R4 directly on the tables.
pub(crate) fn check_consistency(index: &RedbIndex) {
    let rt = index.db().begin_read().unwrap();
    let meta = rt.open_table(META).unwrap();
    let next = meta.get(KEY_NEXT_SLOT).unwrap().unwrap().value();
    let stored = meta.get(KEY_STORED).unwrap().unwrap().value();
    let collections = rt.open_table(COLLECTIONS).unwrap();
    let slots = rt.open_table(SLOTS).unwrap();
    let dead = rt.open_table(DEAD_SLOTS).unwrap();
    let entries = rt.open_table(ENTRIES).unwrap();
    let by_id = rt.open_table(BY_ID).unwrap();
    let urls = rt.open_table(URLS).unwrap();

    let mut slot_ids = HashSet::new();
    for row in slots.iter().unwrap() {
        let (k, _) = row.unwrap();
        assert!(
            k.value() < next,
            "I-R4: slot {} >= next_slot {next}",
            k.value()
        );
        slot_ids.insert(k.value());
    }
    let dead_ids: HashSet<u64> = dead.iter().unwrap().map(|r| r.unwrap().0.value()).collect();
    // I-R1
    for row in collections.iter().unwrap() {
        let (k, v) = row.unwrap();
        let rec = CollRecord::decode(v.value()).unwrap();
        for (g, s) in &rec.gens {
            let sv = slots
                .get(*s)
                .unwrap()
                .expect("I-R1: live slot has a SLOTS row");
            assert_eq!(parse_slot_value(sv.value()).unwrap(), (k.value(), *g));
            assert!(!dead_ids.contains(s), "I-R1: live slot {s} is dead");
        }
    }
    for s in &dead_ids {
        assert!(slot_ids.contains(s), "dead slot {s} without SLOTS row");
    }
    // I-R2 and I-R3
    let slot_of = |key: &[u8]| u64::from_be_bytes(key.first_chunk::<8>().copied().unwrap());
    let mut with_url = 0u64;
    let mut n_entries = 0u64;
    for row in entries.iter().unwrap() {
        let (k, v) = row.unwrap();
        assert!(slot_ids.contains(&slot_of(k.value())), "I-R2: entry");
        let pos = crate::codec::entry_key_position(k.value()).unwrap();
        let e = crate::codec::decode_entry(pos, v.value()).unwrap();
        if e.url.is_some() {
            with_url += 1;
        }
        n_entries += 1;
    }
    for row in by_id.iter().unwrap() {
        let (k, _) = row.unwrap();
        let (s, _) = parse_by_id_key(k.value()).unwrap();
        assert!(slot_ids.contains(&s), "I-R2: by_id");
    }
    for row in urls.iter().unwrap() {
        let (k, _) = row.unwrap();
        assert!(slot_ids.contains(&slot_of(k.value())), "I-R2: urls");
    }
    assert_eq!(stored, n_entries, "I-R3: stored_entries");
    assert_eq!(by_id.len().unwrap(), n_entries, "I-R3: by_id");
    assert_eq!(urls.len().unwrap(), with_url, "I-R3: urls");
}
