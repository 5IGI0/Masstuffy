//! `MemIndex`: reference in-memory implementation of [`SecondaryIndex`].
//!
//! Not for production: queries are linear in the size of the collection's active generation
//! (`by_id` without scope is linear in all entries) and a pattern page allocates O(matches). It
//! exists for tests and as a differential oracle for persistent backends.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::{RwLock, RwLockReadGuard};

use masstuffy_cdx::{Entry, Kind};
use masstuffy_urlkey::UrlPattern;
use masstuffy_warc::WarcDate;

use crate::rank::{self, IdCandidate, UrlKey};
use crate::types::check_limit;
use crate::{
    Checkpoint, CollectionKey, CollectionState, Cursor, Error, Hit, Op, Page, PatternQuery,
    Position, SecondaryIndex,
};

/// In-memory index: one `RwLock<State>`. Writers (`apply`) hold the write lock only for validation
/// and mutation (map operations; entry copies are prepared before). Readers hold the read lock for
/// one query, which gives snapshot isolation. There is no other lock, hence no lock order.
#[derive(Debug, Default)]
pub struct MemIndex {
    state: RwLock<State>,
}

#[derive(Debug, Default)]
struct State {
    collections: BTreeMap<CollectionKey, CollectionState>,
    entries: BTreeMap<EntryKey, Stored>,
}

/// Field order = `Ord`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct EntryKey {
    collection: CollectionKey,
    generation: u32,
    position: Position,
}

const MIN_POS: Position = Position {
    segment: 0,
    offset: 0,
};
const MAX_POS: Position = Position {
    segment: u32::MAX,
    offset: u64::MAX,
};

#[derive(Debug)]
struct Stored {
    entry: Entry<'static>,
    url_key: Option<StoredUrlKey>,
}

/// Owned form of [`UrlKey`], computed once at insert.
#[derive(Debug, PartialEq, Eq)]
enum StoredUrlKey {
    Massaged(String),
    Raw(String),
}

impl StoredUrlKey {
    fn is(&self, k: &UrlKey<'_>) -> bool {
        match (self, k) {
            (StoredUrlKey::Massaged(a), UrlKey::Massaged(b)) => a == b,
            (StoredUrlKey::Raw(a), UrlKey::Raw(b)) => a == b,
            _ => false,
        }
    }
}

fn own(entry: &Entry<'_>) -> Entry<'static> {
    Entry {
        kind: entry.kind,
        segment: entry.segment,
        offset: entry.offset,
        raw_size: entry.raw_size,
        digest: entry.digest,
        date: entry.date,
        warc_type: Cow::Owned(entry.warc_type.to_string()),
        record_id: Cow::Owned(entry.record_id.to_string()),
        url: entry.url.as_deref().map(|u| Cow::Owned(u.to_string())),
    }
}

fn hit(key: &EntryKey, stored: &Stored) -> Hit {
    Hit {
        collection: key.collection,
        generation: key.generation,
        entry: stored.entry.clone(),
    }
}

impl MemIndex {
    /// Empty index.
    #[must_use]
    pub fn new() -> MemIndex {
        MemIndex::default()
    }

    fn read(&self) -> Result<RwLockReadGuard<'_, State>, Error> {
        self.state.read().map_err(|_| Error::Poisoned)
    }
}

impl State {
    /// Entries of the active generation of `collection`, in position order, strictly after `after`.
    fn active_range(
        &self,
        collection: CollectionKey,
        after: Option<Position>,
    ) -> impl Iterator<Item = (&EntryKey, &Stored)> {
        let active = self
            .collections
            .get(&collection)
            .and_then(|s| s.active_generation);
        // `active == None` means nothing is visible: an empty iterator.
        active.into_iter().flat_map(move |generation| {
            let key = |position| EntryKey {
                collection,
                generation,
                position,
            };
            let lo = after.map_or(Bound::Included(key(MIN_POS)), |p| Bound::Excluded(key(p)));
            self.entries.range((lo, Bound::Included(key(MAX_POS))))
        })
    }

    fn is_visible(&self, key: &EntryKey) -> bool {
        self.collections
            .get(&key.collection)
            .is_some_and(|s| s.active_generation == Some(key.generation))
    }
}

impl CollectionState {
    /// Routes a checkpoint (generation already validated against the active one).
    fn set_checkpoint(&mut self, cp: Checkpoint) {
        match self.active_generation {
            Some(a) if cp.generation > a => self.staged_checkpoint = Some(cp),
            _ => self.checkpoint = Some(cp),
        }
    }

    /// Makes `g` active, carrying the checkpoint slots over (A1.2 rule).
    fn activate(&mut self, g: u32) {
        let matching = |c: Option<Checkpoint>| c.filter(|c| c.generation == g);
        let new_main = matching(self.checkpoint).or(matching(self.staged_checkpoint));
        self.staged_checkpoint = self.staged_checkpoint.filter(|c| c.generation > g);
        self.checkpoint = new_main;
        self.active_generation = Some(g);
    }
}

impl State {
    /// Removes the entries of `c` with generation in `from..=to`. The keys are collected first,
    /// then removed.
    fn remove_generations(&mut self, c: CollectionKey, from: u32, to: u32) {
        let key = |generation, position| EntryKey {
            collection: c,
            generation,
            position,
        };
        let keys: Vec<EntryKey> = self
            .entries
            .range(key(from, MIN_POS)..=key(to, MAX_POS))
            .map(|(k, _)| *k)
            .collect();
        for k in keys {
            self.entries.remove(&k);
        }
    }
}

impl SecondaryIndex for MemIndex {
    fn apply(&self, ops: &[Op<'_>]) -> Result<(), Error> {
        // Pass 0, outside the lock: validate inserts and build the owned copies.
        let mut prepared: Vec<Option<Stored>> = Vec::with_capacity(ops.len());
        for (i, op) in ops.iter().enumerate() {
            let stored = if let Op::Insert { entry, .. } = op {
                if entry.kind != Kind::Add {
                    return Err(Error::NotAnAdd { op: i });
                }
                entry
                    .validate()
                    .map_err(|source| Error::InvalidEntry { op: i, source })?;
                let url_key = entry.url.as_deref().map(|u| match rank::url_key(u) {
                    UrlKey::Massaged(k) => StoredUrlKey::Massaged(k),
                    UrlKey::Raw(r) => StoredUrlKey::Raw(r.to_owned()),
                });
                Some(Stored {
                    entry: own(entry),
                    url_key,
                })
            } else {
                None
            };
            prepared.push(stored);
        }

        let mut guard = self.state.write().map_err(|_| Error::Poisoned)?;
        let state = &mut *guard;

        // Pass 1: validate in order against an overlay, without mutating anything.
        let mut overlay: BTreeMap<CollectionKey, Option<CollectionState>> = BTreeMap::new();
        for (i, op) in ops.iter().enumerate() {
            let collection = match op {
                Op::Insert { collection, .. }
                | Op::Tombstone { collection, .. }
                | Op::ReplaceGeneration { collection, .. }
                | Op::DiscardStaged { collection, .. }
                | Op::DropCollection { collection }
                | Op::Checkpoint { collection, .. } => *collection,
            };
            let slot = overlay
                .entry(collection)
                .or_insert_with(|| state.collections.get(&collection).copied());
            let checked = match op {
                Op::Insert { generation, .. }
                | Op::Tombstone { generation, .. }
                | Op::ReplaceGeneration { generation, .. } => Some(*generation),
                Op::Checkpoint { checkpoint, .. } => Some(checkpoint.generation),
                Op::DiscardStaged { .. } | Op::DropCollection { .. } => None,
            };
            if let (Some(st), Some(generation)) = (slot.as_ref(), checked)
                && let Some(active) = st.active_generation
                && generation < active
            {
                return Err(Error::StaleGeneration {
                    op: i,
                    collection,
                    generation,
                    active,
                });
            }
            if let (Op::DiscardStaged { generation, .. }, Some(st)) = (op, slot.as_ref())
                && let Some(active) = st.active_generation
                && *generation <= active
            {
                return Err(Error::NotStaged {
                    op: i,
                    collection,
                    generation: *generation,
                    active,
                });
            }
            match op {
                Op::DropCollection { .. } => *slot = None,
                Op::DiscardStaged { generation, .. } => {
                    if let Some(st) = slot.as_mut() {
                        st.staged_checkpoint =
                            st.staged_checkpoint.filter(|c| c.generation != *generation);
                    }
                }
                _ => {
                    let mut st = slot.unwrap_or_default();
                    match op {
                        Op::ReplaceGeneration { generation, .. }
                            if st.active_generation != Some(*generation) =>
                        {
                            st.activate(*generation);
                        }
                        Op::Checkpoint { checkpoint, .. } => st.set_checkpoint(*checkpoint),
                        _ => {}
                    }
                    *slot = Some(st);
                }
            }
        }

        // Pass 2: mutate. Infallible: no `?`, no unwrap, no indexing.
        for (stored, op) in prepared.iter_mut().zip(ops) {
            match op {
                Op::Insert {
                    collection,
                    generation,
                    entry,
                } => {
                    state.collections.entry(*collection).or_default();
                    if let Some(stored) = stored.take() {
                        let key = EntryKey {
                            collection: *collection,
                            generation: *generation,
                            position: Position {
                                segment: entry.segment,
                                offset: entry.offset,
                            },
                        };
                        state.entries.entry(key).or_insert(stored);
                    }
                }
                Op::Tombstone {
                    collection,
                    generation,
                    position,
                } => {
                    state.collections.entry(*collection).or_default();
                    state.entries.remove(&EntryKey {
                        collection: *collection,
                        generation: *generation,
                        position: *position,
                    });
                }
                Op::ReplaceGeneration {
                    collection,
                    generation,
                } => {
                    let st = state.collections.entry(*collection).or_default();
                    if st.active_generation != Some(*generation) {
                        st.activate(*generation);
                        if let Some(below) = generation.checked_sub(1) {
                            state.remove_generations(*collection, 0, below);
                        }
                    }
                }
                Op::DiscardStaged {
                    collection,
                    generation,
                } => {
                    if let Some(st) = state.collections.get_mut(collection) {
                        st.staged_checkpoint =
                            st.staged_checkpoint.filter(|c| c.generation != *generation);
                        state.remove_generations(*collection, *generation, *generation);
                    }
                }
                Op::DropCollection { collection } => {
                    state.collections.remove(collection);
                    state.remove_generations(*collection, 0, u32::MAX);
                }
                Op::Checkpoint {
                    collection,
                    checkpoint,
                } => {
                    state
                        .collections
                        .entry(*collection)
                        .or_default()
                        .set_checkpoint(*checkpoint);
                }
            }
        }
        Ok(())
    }

    fn state(&self, collection: CollectionKey) -> Result<Option<CollectionState>, Error> {
        Ok(self.read()?.collections.get(&collection).copied())
    }

    fn collections(&self) -> Result<Vec<CollectionKey>, Error> {
        Ok(self.read()?.collections.keys().copied().collect())
    }

    fn by_id(&self, scope: Option<CollectionKey>, record_id: &str) -> Result<Option<Hit>, Error> {
        let state = self.read()?;
        let candidates: Box<dyn Iterator<Item = (&EntryKey, &Stored)> + '_> = match scope {
            Some(c) => Box::new(state.active_range(c, None)),
            None => Box::new(state.entries.iter().filter(|(k, _)| state.is_visible(k))),
        };
        let found: Vec<(&EntryKey, &Stored)> = candidates
            .filter(|(_, s)| &*s.entry.record_id == record_id)
            .collect();
        let picked = rank::pick_by_id(found.iter().map(|(k, s)| IdCandidate {
            collection: k.collection,
            date_nanos: s.entry.date.unix_nanos(),
            position: k.position,
        }));
        Ok(picked.and_then(|p| {
            found
                .into_iter()
                .find(|(k, _)| k.collection == p.collection && k.position == p.position)
                .map(|(k, s)| hit(k, s))
        }))
    }

    fn by_url_nearest(
        &self,
        collection: CollectionKey,
        url: &str,
        date: Option<WarcDate>,
    ) -> Result<Option<Hit>, Error> {
        let wanted = rank::url_key(url);
        let target = date.map(|d| d.unix_nanos());
        let state = self.read()?;
        let best = state
            .active_range(collection, None)
            .filter(|(_, s)| {
                !rank::is_request(&s.entry.warc_type)
                    && s.url_key.as_ref().is_some_and(|k| k.is(&wanted))
            })
            .max_by(|(ka, a), (kb, b)| {
                rank::nearest_cmp(
                    target,
                    (a.entry.date.unix_nanos(), ka.position),
                    (b.entry.date.unix_nanos(), kb.position),
                )
            });
        Ok(best.map(|(k, s)| hit(k, s)))
    }

    fn by_pattern(
        &self,
        collection: CollectionKey,
        pattern: &UrlPattern,
        query: &PatternQuery,
    ) -> Result<Page, Error> {
        check_limit(query.limit)?;
        let state = self.read()?;
        let mut cands: Vec<(&str, i128, Position, &EntryKey, &Stored)> = state
            .active_range(collection, None)
            .filter_map(|(k, s)| match &s.url_key {
                Some(StoredUrlKey::Massaged(m)) if pattern.matches(m) => {
                    Some((m.as_str(), s.entry.date.unix_nanos(), k.position, k, s))
                }
                _ => None,
            })
            .filter(|(m, n, p, ..)| {
                query
                    .after
                    .as_ref()
                    .is_none_or(|c| (*m, *n, *p) > (c.key.as_str(), c.date_nanos, c.position))
            })
            .collect();
        cands.sort_unstable_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
        let more = cands.len() > query.limit;
        cands.truncate(query.limit);
        let hits: Vec<Hit> = cands.iter().map(|c| hit(c.3, c.4)).collect();
        let next = if more {
            cands
                .last()
                .zip(hits.last())
                .map(|(c, h)| Cursor::of(c.0, h))
        } else {
            None
        };
        Ok(Page { hits, next })
    }

    fn list(
        &self,
        collection: CollectionKey,
        after: Option<Position>,
        limit: usize,
    ) -> Result<Vec<Hit>, Error> {
        check_limit(limit)?;
        let state = self.read()?;
        Ok(state
            .active_range(collection, after)
            .take(limit)
            .map(|(k, s)| hit(k, s))
            .collect())
    }

    fn purge(&self, max_entries: usize) -> Result<usize, Error> {
        if max_entries == 0 {
            return Err(Error::InvalidLimit {
                limit: 0,
                max: usize::MAX,
            });
        }
        // Deletes are eager: there is never anything to reclaim.
        Ok(0)
    }

    fn stored_entries(&self) -> Result<u64, Error> {
        // usize -> u64 cannot fail on supported targets; saturate rather than cast.
        Ok(u64::try_from(self.read()?.entries.len()).unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::{add, date, key};

    fn insert(c: CollectionKey, g: u32, s: u32, o: u64, id: &str) -> Op<'static> {
        Op::Insert {
            collection: c,
            generation: g,
            entry: add(
                s,
                o,
                id,
                Some("http://e.test/"),
                date(2020, 1, 1, 0, 0, 0),
                "response",
            ),
        }
    }

    #[test]
    fn purge_is_noop_eager() {
        let m = MemIndex::new();
        let (c, d) = (key(1), key(2));
        m.apply(&[
            Op::ReplaceGeneration {
                collection: c,
                generation: 1,
            },
            insert(c, 1, 1, 0, "a"),
            insert(c, 2, 1, 0, "b"),
            insert(d, 1, 1, 0, "c"),
        ])
        .unwrap();
        m.apply(&[Op::ReplaceGeneration {
            collection: c,
            generation: 2,
        }])
        .unwrap();
        assert_eq!(m.stored_entries().unwrap(), 2);
        m.apply(&[Op::DropCollection { collection: c }]).unwrap();
        assert_eq!(m.stored_entries().unwrap(), 1);
        assert_eq!(m.purge(10).unwrap(), 0);
    }

    #[test]
    fn object_safe() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        let _: Box<dyn SecondaryIndex> = Box::new(MemIndex::new());
        assert_send_sync::<MemIndex>();
        assert_send_sync::<Error>();
    }

    #[test]
    fn concurrent_apply_and_query_tasks() {
        use std::sync::mpsc;
        use std::time::Duration;
        let m = MemIndex::new();
        let (tx, rx) = mpsc::channel();
        std::thread::scope(|s| {
            for t in 0u128..8 {
                let (m, tx) = (&m, tx.clone());
                s.spawn(move || {
                    let c = key(100 + t);
                    m.apply(&[Op::ReplaceGeneration {
                        collection: c,
                        generation: 1,
                    }])
                    .unwrap();
                    let pat = masstuffy_urlkey::UrlPattern::new(
                        "e.test",
                        masstuffy_urlkey::HostMatch::Exact,
                        masstuffy_urlkey::PortFilter::Any,
                        masstuffy_urlkey::PathFilter::Any,
                    )
                    .unwrap();
                    for i in 0..100u32 {
                        m.apply(&[insert(c, 1, 1, u64::from(i) * 10, &format!("id{t}-{i}"))])
                            .unwrap();
                        let other = key(100 + (t + 1) % 8);
                        m.by_id(Some(other), "id0-0").unwrap();
                        m.by_pattern(other, &pat, &PatternQuery::new(10)).unwrap();
                    }
                    tx.send(()).unwrap();
                });
            }
            drop(tx);
            // Best-effort timeout: `thread::scope` joins first, so this cannot fire on a deadlock.
            for _ in 0..8 {
                rx.recv_timeout(Duration::from_secs(60)).unwrap();
            }
        });
        for t in 0u128..8 {
            assert_eq!(crate::conformance::dump(&m, key(100 + t)).len(), 100);
        }
        assert_eq!(m.stored_entries().unwrap(), 800);
    }
}
