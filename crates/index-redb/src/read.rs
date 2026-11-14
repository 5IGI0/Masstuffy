//! Queries. Each one is a single redb read transaction (an MVCC snapshot, never blocked by the
//! writer) that opens only the tables it needs.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::ops::Bound;

use masstuffy_index::rank::{self, IdCandidate, UrlKey};
use masstuffy_index::{
    CollectionKey, CollectionState, Cursor, Error, Hit, MAX_LIMIT, Page, PatternQuery, Position,
    UrlPattern, WarcDate,
};
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata};

use crate::RedbIndex;
use crate::codec::{
    CollRecord, MAX_POS, MIN_POS, bound_key, by_id_bounds, class0_bound, cursor_bound,
    decode_entry, enc_date, entry_key, parse_by_id_key, parse_massaged_row, parse_slot_value,
    row_tail, url_part, url_prefix,
};
use crate::error::{backend, corrupt};
use crate::schema::{BY_ID, COLLECTIONS, ENTRIES, KEY_STORED, META, SLOTS, URLS};

type EntriesRo = redb::ReadOnlyTable<&'static [u8], &'static [u8]>;
type CollectionsRo = redb::ReadOnlyTable<u128, &'static [u8]>;

fn check_limit(limit: usize) -> Result<(), Error> {
    if (1..=MAX_LIMIT).contains(&limit) {
        Ok(())
    } else {
        Err(Error::InvalidLimit {
            limit,
            max: MAX_LIMIT,
        })
    }
}

fn record(t: &CollectionsRo, c: CollectionKey) -> Result<Option<CollRecord>, Error> {
    match t.get(c.get()).map_err(backend)? {
        Some(g) => Ok(Some(CollRecord::decode(g.value())?)),
        None => Ok(None),
    }
}

/// Active slot and generation of `c`.
fn active(rt: &redb::ReadTransaction, c: CollectionKey) -> Result<Option<(u64, u32)>, Error> {
    let t = rt.open_table(COLLECTIONS).map_err(backend)?;
    Ok(record(&t, c)?.and_then(|r| {
        let slot = r.active_slot()?;
        Some((slot, r.state.active_generation?))
    }))
}

fn hit_at(
    entries: &EntriesRo,
    c: CollectionKey,
    generation: u32,
    slot: u64,
    pos: Position,
) -> Result<Hit, Error> {
    let k = entry_key(slot, pos);
    let v = entries
        .get(k.as_slice())
        .map_err(backend)?
        .ok_or_else(|| corrupt("dangling url row"))?;
    let entry = decode_entry(pos, v.value())?.into_owned();
    Ok(Hit {
        collection: c,
        generation,
        entry,
    })
}

fn as_slice_bound(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match b {
        Bound::Included(v) => Bound::Included(v.as_slice()),
        Bound::Excluded(v) => Bound::Excluded(v.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

pub(crate) fn state(index: &RedbIndex, c: CollectionKey) -> Result<Option<CollectionState>, Error> {
    let rt = index.db.begin_read().map_err(backend)?;
    let t = rt.open_table(COLLECTIONS).map_err(backend)?;
    Ok(record(&t, c)?.map(|r| r.state))
}

pub(crate) fn collections(index: &RedbIndex) -> Result<Vec<CollectionKey>, Error> {
    let rt = index.db.begin_read().map_err(backend)?;
    let t = rt.open_table(COLLECTIONS).map_err(backend)?;
    let n = usize::try_from(t.len().map_err(backend)?).unwrap_or(0);
    let mut out = Vec::with_capacity(n);
    for row in t.iter().map_err(backend)? {
        let (k, _) = row.map_err(backend)?;
        out.push(CollectionKey::new(k.value()));
    }
    Ok(out)
}

pub(crate) fn stored_entries(index: &RedbIndex) -> Result<u64, Error> {
    let rt = index.db.begin_read().map_err(backend)?;
    let t = rt.open_table(META).map_err(backend)?;
    t.get(KEY_STORED)
        .map_err(backend)?
        .map(|g| g.value())
        .ok_or_else(|| corrupt("meta has no stored_entries"))
}

pub(crate) fn list(
    index: &RedbIndex,
    c: CollectionKey,
    after: Option<Position>,
    limit: usize,
) -> Result<Vec<Hit>, Error> {
    check_limit(limit)?;
    let rt = index.db.begin_read().map_err(backend)?;
    let Some((slot, generation)) = active(&rt, c)? else {
        return Ok(Vec::new());
    };
    let entries = rt.open_table(ENTRIES).map_err(backend)?;
    let lo = entry_key(slot, after.unwrap_or(MIN_POS));
    let hi = entry_key(slot, MAX_POS);
    let lower = if after.is_some() {
        Bound::Excluded(lo.as_slice())
    } else {
        Bound::Included(lo.as_slice())
    };
    let mut out = Vec::with_capacity(limit.min(256));
    for row in entries
        .range::<&[u8]>((lower, Bound::Included(hi.as_slice())))
        .map_err(backend)?
        .take(limit)
    {
        let (k, v) = row.map_err(backend)?;
        let pos = crate::codec::entry_key_position(k.value())?;
        out.push(Hit {
            collection: c,
            generation,
            entry: decode_entry(pos, v.value())?.into_owned(),
        });
    }
    Ok(out)
}

/// First row of `rows` that is not a `request`, as `(date nanos, position)`.
fn first_non_request<'a, I>(rows: I) -> Result<Option<(i128, Position)>, Error>
where
    I: Iterator<
        Item = Result<
            (
                redb::AccessGuard<'a, &'static [u8]>,
                redb::AccessGuard<'a, u8>,
            ),
            redb::StorageError,
        >,
    >,
{
    for row in rows {
        let (k, v) = row.map_err(backend)?;
        if v.value() & 1 == 0 {
            return Ok(Some(row_tail(k.value())?));
        }
    }
    Ok(None)
}

/// `prefix ‖ tail` with `tail` of `n` copies of `byte` after `date`.
fn bound_with(prefix: &[u8], date: Option<[u8; 16]>, fill: u8) -> Vec<u8> {
    let mut v = Vec::with_capacity(prefix.len().saturating_add(28));
    v.extend_from_slice(prefix);
    let fixed = match date {
        Some(d) => {
            v.extend_from_slice(&d);
            12
        }
        None => 28,
    };
    v.resize(v.len().saturating_add(fixed), fill);
    v
}

pub(crate) fn by_url_nearest(
    index: &RedbIndex,
    c: CollectionKey,
    url: &str,
    date: Option<WarcDate>,
) -> Result<Option<Hit>, Error> {
    let key = rank::url_key(url);
    if let UrlKey::Raw(r) = &key
        && u32::try_from(r.len()).is_err()
    {
        // Cannot be stored, so there is no match.
        return Ok(None);
    }
    let rt = index.db.begin_read().map_err(backend)?;
    let Some((slot, generation)) = active(&rt, c)? else {
        return Ok(None);
    };
    let prefix = url_prefix(slot, &url_part(&key)?);
    let urls = rt.open_table(URLS).map_err(backend)?;
    let lo = bound_with(&prefix, None, 0x00);
    let hi = bound_with(&prefix, None, 0xFF);
    let best: Option<(i128, Position)> = match date {
        None => first_non_request(
            urls.range::<&[u8]>(lo.as_slice()..=hi.as_slice())
                .map_err(backend)?
                .rev(),
        )?,
        Some(d) => {
            let t = d.unix_nanos();
            let mid = bound_with(&prefix, Some(enc_date(t)), 0xFF);
            let below = first_non_request(
                urls.range::<&[u8]>(lo.as_slice()..=mid.as_slice())
                    .map_err(backend)?
                    .rev(),
            )?;
            let above = match first_non_request(
                urls.range::<&[u8]>((
                    Bound::Excluded(mid.as_slice()),
                    Bound::Included(hi.as_slice()),
                ))
                .map_err(backend)?,
            )? {
                Some((d1, _)) => {
                    // Greatest position at the smallest later date, as `nearest_cmp` requires.
                    let a = bound_with(&prefix, Some(enc_date(d1)), 0x00);
                    let b = bound_with(&prefix, Some(enc_date(d1)), 0xFF);
                    first_non_request(
                        urls.range::<&[u8]>(a.as_slice()..=b.as_slice())
                            .map_err(backend)?
                            .rev(),
                    )?
                }
                None => None,
            };
            match (below, above) {
                (Some(a), Some(b)) => {
                    if rank::nearest_cmp(Some(t), a, b) == Ordering::Greater {
                        Some(a)
                    } else {
                        Some(b)
                    }
                }
                (a, b) => a.or(b),
            }
        }
    };
    let Some((_, pos)) = best else {
        return Ok(None);
    };
    let entries = rt.open_table(ENTRIES).map_err(backend)?;
    Ok(Some(hit_at(&entries, c, generation, slot, pos)?))
}

pub(crate) fn by_pattern(
    index: &RedbIndex,
    c: CollectionKey,
    pattern: &UrlPattern,
    query: &PatternQuery,
) -> Result<Page, Error> {
    check_limit(query.limit)?;
    let rt = index.db.begin_read().map_err(backend)?;
    let Some((slot, generation)) = active(&rt, c)? else {
        return Ok(Page {
            hits: Vec::new(),
            next: None,
        });
    };
    let urls = rt.open_table(URLS).map_err(backend)?;
    let entries = rt.open_table(ENTRIES).map_err(backend)?;
    let mut hits: Vec<Hit> = Vec::with_capacity(query.limit.min(256));
    let mut last_key = String::new();
    let mut more = false;
    'outer: for r in pattern.key_ranges() {
        let lo = class0_bound(slot, &r.start);
        let hi = class0_bound(slot, &r.end);
        let cb = query.after.as_ref().map(|cur| cursor_bound(slot, cur));
        let mut lower = Bound::Included(lo.as_slice());
        if let Some(cb) = &cb {
            if bound_key(cb) >= hi.as_slice() {
                // The whole range is at or before the cursor.
                continue;
            }
            if bound_key(cb) >= lo.as_slice() {
                lower = as_slice_bound(cb);
            }
        }
        for row in urls
            .range::<&[u8]>((lower, Bound::Excluded(hi.as_slice())))
            .map_err(backend)?
        {
            let (k, _) = row.map_err(backend)?;
            let (key, _, pos) = parse_massaged_row(k.value())?;
            if !pattern.matches(key) {
                continue;
            }
            if hits.len() == query.limit {
                more = true;
                break 'outer;
            }
            hits.push(hit_at(&entries, c, generation, slot, pos)?);
            last_key.clear();
            last_key.push_str(key);
        }
    }
    let next = if more {
        hits.last().map(|h| Cursor::of(&last_key, h))
    } else {
        None
    };
    Ok(Page { hits, next })
}

pub(crate) fn by_id(
    index: &RedbIndex,
    scope: Option<CollectionKey>,
    record_id: &str,
) -> Result<Option<Hit>, Error> {
    struct Cand {
        generation: u32,
        slot: u64,
        cand: IdCandidate,
    }
    if record_id.as_bytes().contains(&0) {
        // Record ids are atoms without NUL; a NUL would break the prefix scan below.
        return Ok(None);
    }
    let rt = index.db.begin_read().map_err(backend)?;
    let by_id = rt.open_table(BY_ID).map_err(backend)?;
    let slots = rt.open_table(SLOTS).map_err(backend)?;
    let collections = rt.open_table(COLLECTIONS).map_err(backend)?;
    let (lo, hi) = by_id_bounds(record_id);
    let mut slot_cache: HashMap<u64, (CollectionKey, u32)> = HashMap::new();
    let mut active_cache: HashMap<CollectionKey, Option<u64>> = HashMap::new();
    let mut cands: Vec<Cand> = Vec::new();
    for row in by_id
        .range::<&[u8]>(lo.as_slice()..hi.as_slice())
        .map_err(backend)?
    {
        let (k, v) = row.map_err(backend)?;
        if k.value().len() != record_id.len().saturating_add(21) {
            // Defence in depth: the row belongs to another id.
            continue;
        }
        let (slot, position) = parse_by_id_key(k.value())?;
        let date = <[u8; 16]>::try_from(v.value()).map_err(|_| corrupt("malformed by_id date"))?;
        let (collection, generation) = match slot_cache.get(&slot) {
            Some(x) => *x,
            None => {
                let g = slots
                    .get(slot)
                    .map_err(backend)?
                    .ok_or_else(|| corrupt("by_id row of an unknown slot"))?;
                let (c, generation) = parse_slot_value(g.value())?;
                let x = (CollectionKey::new(c), generation);
                slot_cache.insert(slot, x);
                x
            }
        };
        if scope.is_some_and(|s| s != collection) {
            continue;
        }
        let active_slot = match active_cache.get(&collection) {
            Some(a) => *a,
            None => {
                let a = record(&collections, collection)?.and_then(|r| r.active_slot());
                active_cache.insert(collection, a);
                a
            }
        };
        if active_slot != Some(slot) {
            continue;
        }
        cands.push(Cand {
            generation,
            slot,
            cand: IdCandidate {
                collection,
                date_nanos: crate::codec::dec_date(date),
                position,
            },
        });
    }
    let Some(best) = rank::pick_by_id(cands.iter().map(|x| x.cand)) else {
        return Ok(None);
    };
    let Some(w) = cands
        .iter()
        .find(|x| x.cand.collection == best.collection && x.cand.position == best.position)
    else {
        return Ok(None);
    };
    let entries = rt.open_table(ENTRIES).map_err(backend)?;
    Ok(Some(hit_at(
        &entries,
        w.cand.collection,
        w.generation,
        w.slot,
        w.cand.position,
    )?))
}
