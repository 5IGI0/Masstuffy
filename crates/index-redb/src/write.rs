//! Write path: `apply`, `purge`, `flush`, durability choice.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry as MapEntry;
use std::ops::Bound;
use std::sync::atomic::Ordering::Relaxed;

use masstuffy_index::rank;
use masstuffy_index::{CollectionKey, Error, Op, Position};
use redb::ReadableTable;

use crate::RedbIndex;
use crate::codec::{
    CollRecord, MAX_POS, MIN_POS, by_id_key, decode_entry, enc_date, encode_entry, entry_key,
    entry_key_position, slot_value, url_part, url_prefix, url_row,
};
use crate::error::{backend, corrupt};
use crate::schema::{
    BY_ID, COLLECTIONS, DEAD_SLOTS, ENTRIES, KEY_NEXT_SLOT, KEY_STORED, META, SLOTS, URLS,
};

/// Everything of an `Insert` that can be computed before the write transaction starts.
struct Prepared {
    value: Vec<u8>,
    /// `(url part, flags)` if the entry has a URL.
    url: Option<(Vec<u8>, u8)>,
    date: [u8; 16],
}

impl RedbIndex {
    /// Takes the write gate. Held from `begin` until after the counter update, so `unsynced` is
    /// only touched by one writer at a time (redb's own writer lock is released inside `commit`,
    /// before the counter is updated).
    fn gate(&self) -> Result<std::sync::MutexGuard<'_, ()>, Error> {
        self.gate.lock().map_err(|_| Error::Poisoned)
    }

    /// Starts the write transaction and decides its durability. The caller holds the gate.
    fn begin(&self, force_durable: bool) -> Result<(redb::WriteTransaction, bool), Error> {
        let mut txn = self.db.begin_write().map_err(backend)?;
        let n = self.unsynced.load(Relaxed);
        let durable = force_durable || n.saturating_add(1) >= self.sync_every.get();
        txn.set_quick_repair(self.quick_repair);
        if !durable {
            txn.set_durability(redb::Durability::None)
                .map_err(backend)?;
        }
        Ok((txn, durable))
    }

    /// Records a successful commit.
    fn committed(&self, durable: bool) {
        if durable {
            self.unsynced.store(0, Relaxed);
        } else {
            let n = self.unsynced.load(Relaxed);
            self.unsynced.store(n.saturating_add(1), Relaxed);
        }
    }

    /// Commits `txn` if `r` is `Ok`, aborts it otherwise (nothing changed on error).
    fn finish(
        &self,
        txn: redb::WriteTransaction,
        durable: bool,
        r: Result<(), Error>,
    ) -> Result<(), Error> {
        match r {
            Ok(()) => {
                txn.commit().map_err(backend)?;
                self.committed(durable);
                Ok(())
            }
            Err(e) => {
                // The original error matters; an uncommitted transaction changes nothing.
                let _ = txn.abort();
                Err(e)
            }
        }
    }
}

fn prepare(op: usize, entry: &masstuffy_cdx::Entry<'_>) -> Result<Prepared, Error> {
    rank::validate_insert(op, entry)?;
    let mut value = Vec::new();
    encode_entry(entry, &mut value)?;
    let url = match entry.url.as_deref() {
        Some(u) => {
            let part = url_part(&rank::url_key(u))?;
            Some((part, u8::from(rank::is_request(&entry.warc_type))))
        }
        None => None,
    };
    Ok(Prepared {
        value,
        url,
        date: enc_date(entry.date.unix_nanos()),
    })
}

pub(crate) fn apply(index: &RedbIndex, ops: &[Op<'_>]) -> Result<(), Error> {
    if ops.is_empty() {
        return Ok(());
    }
    // Prepare outside the write lock: every insert validation error is found before any state
    // error, as in MemIndex.
    let mut prepared: Vec<Option<Prepared>> = Vec::with_capacity(ops.len());
    for (i, op) in ops.iter().enumerate() {
        prepared.push(match op {
            Op::Insert { entry, .. } => Some(prepare(i, entry)?),
            _ => None,
        });
    }
    let force = ops.iter().any(|op| {
        matches!(
            op,
            Op::ReplaceGeneration { .. } | Op::DiscardStaged { .. } | Op::DropCollection { .. }
        )
    });
    let _gate = index.gate()?;
    let (txn, durable) = index.begin(force)?;
    let r = apply_in(&txn, ops, &prepared);
    index.finish(txn, durable, r)
}

fn meta_u64(meta: &redb::Table<'_, &str, u64>, key: &str) -> Result<u64, Error> {
    meta.get(key)
        .map_err(backend)?
        .map(|g| g.value())
        .ok_or_else(|| corrupt(format!("meta has no {key}")))
}

fn load_record(
    collections: &redb::Table<'_, u128, &[u8]>,
    c: CollectionKey,
) -> Result<Option<CollRecord>, Error> {
    match collections.get(c.get()).map_err(backend)? {
        Some(g) => Ok(Some(CollRecord::decode(g.value())?)),
        None => Ok(None),
    }
}

/// Allocates a fresh slot for `(c, g)`.
fn allocate(
    slots: &mut redb::Table<'_, u64, &[u8]>,
    next_slot: &mut u64,
    c: CollectionKey,
    g: u32,
) -> Result<u64, Error> {
    let s = *next_slot;
    *next_slot = s
        .checked_add(1)
        .ok_or_else(|| corrupt("slot counter overflow"))?;
    slots
        .insert(s, slot_value(c.get(), g).as_slice())
        .map_err(backend)?;
    Ok(s)
}

/// Body of `apply` inside the write transaction. Tables are dropped on return, before the commit.
/// Any error returns early: the caller aborts the transaction, so nothing changed.
fn apply_in(
    txn: &redb::WriteTransaction,
    ops: &[Op<'_>],
    prepared: &[Option<Prepared>],
) -> Result<(), Error> {
    let mut meta = txn.open_table(META).map_err(backend)?;
    let mut collections = txn.open_table(COLLECTIONS).map_err(backend)?;
    let mut slots = txn.open_table(SLOTS).map_err(backend)?;
    let mut dead = txn.open_table(DEAD_SLOTS).map_err(backend)?;
    let mut entries = txn.open_table(ENTRIES).map_err(backend)?;
    let mut by_id = txn.open_table(BY_ID).map_err(backend)?;
    let mut urls = txn.open_table(URLS).map_err(backend)?;

    let mut next_slot = meta_u64(&meta, KEY_NEXT_SLOT)?;
    let mut stored = meta_u64(&meta, KEY_STORED)?;
    let mut cache: BTreeMap<CollectionKey, Option<CollRecord>> = BTreeMap::new();

    for (i, (op, prep)) in ops.iter().zip(prepared).enumerate() {
        let c = op.collection();
        let rec = match cache.entry(c) {
            MapEntry::Occupied(o) => o.into_mut(),
            MapEntry::Vacant(v) => v.insert(load_record(&collections, c)?),
        };
        rank::check_op(i, rec.as_ref().map(|r| &r.state), op)?;
        let new_state = rank::step_state(rec.as_ref().map(|r| r.state), op);
        match op {
            Op::Insert {
                generation: g,
                entry,
                ..
            } => {
                let r = rec.get_or_insert_with(CollRecord::default);
                let slot = match r.slot_of(*g) {
                    Some(s) => s,
                    None => {
                        let s = allocate(&mut slots, &mut next_slot, c, *g)?;
                        r.push_slot(*g, s);
                        s
                    }
                };
                let pos = Position {
                    segment: entry.segment,
                    offset: entry.offset,
                };
                let k = entry_key(slot, pos);
                let exists = entries.get(k.as_slice()).map_err(backend)?.is_some();
                if !exists && let Some(p) = prep {
                    entries
                        .insert(k.as_slice(), p.value.as_slice())
                        .map_err(backend)?;
                    by_id
                        .insert(
                            by_id_key(&entry.record_id, slot, pos).as_slice(),
                            p.date.as_slice(),
                        )
                        .map_err(backend)?;
                    if let Some((part, flags)) = &p.url {
                        urls.insert(
                            url_row(&url_prefix(slot, part), p.date, pos).as_slice(),
                            *flags,
                        )
                        .map_err(backend)?;
                    }
                    stored = stored
                        .checked_add(1)
                        .ok_or_else(|| corrupt("stored entry counter overflow"))?;
                }
            }
            Op::Tombstone {
                generation: g,
                position,
                ..
            } => {
                // No slot: nothing stored, nothing to remove (and no slot is allocated).
                if let Some(slot) = rec.as_ref().and_then(|r| r.slot_of(*g)) {
                    let k = entry_key(slot, *position);
                    let found = match entries.get(k.as_slice()).map_err(backend)? {
                        Some(v) => {
                            let e = decode_entry(*position, v.value())?;
                            Some((
                                e.record_id.into_owned(),
                                e.url.map(std::borrow::Cow::into_owned),
                                e.date.unix_nanos(),
                            ))
                        }
                        None => None,
                    };
                    if let Some((rid, url, nanos)) = found {
                        entries.remove(k.as_slice()).map_err(backend)?;
                        by_id
                            .remove(by_id_key(&rid, slot, *position).as_slice())
                            .map_err(backend)?;
                        if let Some(u) = url {
                            let part = url_part(&rank::url_key(&u))?;
                            let row = url_row(&url_prefix(slot, &part), enc_date(nanos), *position);
                            if urls.remove(row.as_slice()).map_err(backend)?.is_none() {
                                // The massaged key drifted without a SCHEMA_VERSION bump.
                                return Err(corrupt("url row missing for a stored entry"));
                            }
                        }
                        stored = stored
                            .checked_sub(1)
                            .ok_or_else(|| corrupt("stored entry counter underflow"))?;
                    }
                }
            }
            Op::ReplaceGeneration { generation: g, .. } => {
                let r = rec.get_or_insert_with(CollRecord::default);
                if r.state.active_generation != Some(*g) {
                    if r.slot_of(*g).is_none() {
                        let s = allocate(&mut slots, &mut next_slot, c, *g)?;
                        r.push_slot(*g, s);
                    }
                    for (_, s2) in r.gens.iter().filter(|(g2, _)| g2 < g) {
                        dead.insert(*s2, ()).map_err(backend)?;
                    }
                    r.gens.retain(|(g2, _)| g2 >= g);
                }
            }
            Op::DiscardStaged { generation: g, .. } => {
                if let Some(r) = rec.as_mut()
                    && let Some(s) = r.slot_of(*g)
                {
                    dead.insert(s, ()).map_err(backend)?;
                    r.gens.retain(|(g2, _)| g2 != g);
                }
            }
            Op::DropCollection { .. } => {
                if let Some(r) = rec.as_ref() {
                    for (_, s) in &r.gens {
                        dead.insert(*s, ()).map_err(backend)?;
                    }
                }
            }
            Op::Checkpoint { .. } => {}
        }
        // The record after the op: gens from the (possibly just created) record, state from the
        // shared rule. A drop discards the record (its slots are all dead already).
        let gens = rec.take().map(|r| r.gens).unwrap_or_default();
        *rec = new_state.map(|state| CollRecord { state, gens });
    }

    for (c, rec) in &cache {
        match rec {
            Some(r) => {
                collections
                    .insert(c.get(), r.encode().as_slice())
                    .map_err(backend)?;
            }
            None => {
                collections.remove(c.get()).map_err(backend)?;
            }
        }
    }
    meta.insert(KEY_NEXT_SLOT, next_slot).map_err(backend)?;
    meta.insert(KEY_STORED, stored).map_err(backend)?;
    Ok(())
}

pub(crate) fn purge(index: &RedbIndex, max_entries: usize) -> Result<usize, Error> {
    if max_entries == 0 {
        return Err(Error::InvalidLimit {
            limit: 0,
            max: usize::MAX,
        });
    }
    let _gate = index.gate()?;
    let (txn, durable) = index.begin(false)?;
    let mut total = 0usize;
    let r = purge_in(&txn, max_entries, &mut total);
    index.finish(txn, durable, r)?;
    Ok(total)
}

/// Deletes up to `max_entries` rows of the oldest dead slots (plus their `by_id`/`urls` rows).
fn purge_in(
    txn: &redb::WriteTransaction,
    max_entries: usize,
    total: &mut usize,
) -> Result<(), Error> {
    let mut meta = txn.open_table(META).map_err(backend)?;
    let mut slots = txn.open_table(SLOTS).map_err(backend)?;
    let mut dead = txn.open_table(DEAD_SLOTS).map_err(backend)?;
    let mut entries = txn.open_table(ENTRIES).map_err(backend)?;
    let mut by_id = txn.open_table(BY_ID).map_err(backend)?;
    let mut urls = txn.open_table(URLS).map_err(backend)?;
    let stored = meta_u64(&meta, KEY_STORED)?;

    let mut budget = max_entries;
    loop {
        let s = match dead.first().map_err(backend)? {
            Some((k, _)) => k.value(),
            None => break,
        };
        let lo = entry_key(s, MIN_POS);
        let hi = entry_key(s, MAX_POS);
        let mut n = 0usize;
        {
            let iter = entries
                .extract_from_if::<&[u8], _>(lo.as_slice()..=hi.as_slice(), |_, _| true)
                .map_err(backend)?;
            for item in iter.take(budget) {
                let (k, v) = item.map_err(backend)?;
                let pos = entry_key_position(k.value())?;
                let e = decode_entry(pos, v.value())?;
                by_id
                    .remove(by_id_key(&e.record_id, s, pos).as_slice())
                    .map_err(backend)?;
                // URL rows are keyed by URL first, so the rows of the purged entries are found by
                // recomputing their key (a count-based range delete would remove other entries' rows
                // on a partial purge). The `SCHEMA_VERSION` rule covers a change of `massage`.
                if let Some(u) = e.url.as_deref() {
                    let part = url_part(&rank::url_key(u))?;
                    urls.remove(
                        url_row(&url_prefix(s, &part), enc_date(e.date.unix_nanos()), pos)
                            .as_slice(),
                    )
                    .map_err(backend)?;
                }
                n = n.saturating_add(1);
            }
        }
        let ulo = s.to_be_bytes();
        let uhi = s
            .checked_add(1)
            .ok_or_else(|| corrupt("slot id overflow"))?
            .to_be_bytes();
        let bounds = (
            Bound::Included(ulo.as_slice()),
            Bound::Excluded(uhi.as_slice()),
        );
        budget = budget.saturating_sub(n);
        *total = total.saturating_add(n);
        let empty = entries
            .range::<&[u8]>(lo.as_slice()..=hi.as_slice())
            .map_err(backend)?
            .next()
            .is_none();
        if empty {
            // Leftover URL rows can only come from corruption: drain them, uncounted.
            {
                let iter = urls
                    .extract_from_if::<&[u8], _>(bounds, |_, _| true)
                    .map_err(backend)?;
                for item in iter {
                    item.map_err(backend)?;
                }
            }
            slots.remove(s).map_err(backend)?;
            dead.remove(s).map_err(backend)?;
            continue;
        }
        break;
    }
    let removed = u64::try_from(*total).map_err(|_| corrupt("purge count overflow"))?;
    let stored = stored
        .checked_sub(removed)
        .ok_or_else(|| corrupt("stored entry counter underflow"))?;
    meta.insert(KEY_STORED, stored).map_err(backend)?;
    Ok(())
}

pub(crate) fn flush(index: &RedbIndex) -> Result<(), Error> {
    let _gate = index.gate()?;
    if index.unsynced.load(Relaxed) == 0 {
        return Ok(());
    }
    let (txn, durable) = index.begin(true)?;
    index.finish(txn, durable, Ok(()))
}
