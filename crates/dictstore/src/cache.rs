//! Byte-bounded LRU of prepared dictionaries, with in-flight load deduplication.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use masstuffy_zframe::{DecoderDict, EncoderDict};

use crate::error::Error;

/// Cache key: a decoder per id, an encoder per (id, level).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Key {
    Dec(u32),
    Enc(u32, i32),
}

impl Key {
    fn dict_id(self) -> u32 {
        match self {
            Key::Dec(id) | Key::Enc(id, _) => id,
        }
    }
}

/// A prepared dictionary of either kind.
#[derive(Clone)]
pub(crate) enum Prepared {
    Dec(Arc<DecoderDict>),
    Enc(Arc<EncoderDict>),
}

impl Prepared {
    fn mem_size(&self) -> u64 {
        match self {
            Prepared::Dec(d) => d.mem_size(),
            Prepared::Enc(e) => e.mem_size(),
        }
    }
}

type Cell = Arc<OnceLock<Option<Prepared>>>;

enum Slot {
    Ready {
        value: Prepared,
        size: u64,
        last_used: u64,
    },
    /// Being built by one thread; the others wait on the cell.
    Loading(Cell),
}

/// The cache state. The per-slot bookkeeping overhead is not counted in the budget.
pub(crate) struct Cache {
    budget: u64,
    /// Invariant: the sum of the `Ready` sizes, and `<= budget`.
    used: u64,
    /// Wrapping LRU clock.
    tick: u64,
    slots: HashMap<Key, Slot>,
}

impl Cache {
    pub(crate) fn new(budget: u64) -> Cache {
        Cache {
            budget,
            used: 0,
            tick: 0,
            slots: HashMap::new(),
        }
    }

    /// `(Ready entries, bytes, budget)`.
    pub(crate) fn stats(&self) -> (usize, u64, u64) {
        let entries = self
            .slots
            .values()
            .filter(|s| matches!(s, Slot::Ready { .. }))
            .count();
        (entries, self.used, self.budget)
    }

    /// Inserts `p` under `key` (whose `Loading` slot was just removed), evicting least recently
    /// used entries into `evicted` (dropped by the caller outside the lock). An entry larger than
    /// the whole budget is not cached.
    fn insert(&mut self, key: Key, p: &Prepared, evicted: &mut Vec<Prepared>) {
        let size = p.mem_size();
        if size > self.budget {
            return;
        }
        while self
            .used
            .checked_add(size)
            .is_none_or(|total| total > self.budget)
        {
            let oldest = self
                .slots
                .iter()
                .filter_map(|(k, s)| match s {
                    Slot::Ready { last_used, .. } => Some((*last_used, *k)),
                    Slot::Loading(_) => None,
                })
                .min_by_key(|(last_used, _)| *last_used)
                .map(|(_, k)| k);
            let Some(oldest) = oldest else { break };
            if let Some(Slot::Ready { value, size, .. }) = self.slots.remove(&oldest) {
                self.used = self.used.saturating_sub(size);
                evicted.push(value);
            }
        }
        if let Some(used) = self.used.checked_add(size).filter(|u| *u <= self.budget) {
            self.slots.insert(
                key,
                Slot::Ready {
                    value: p.clone(),
                    size,
                    last_used: self.tick,
                },
            );
            self.tick = self.tick.wrapping_add(1);
            self.used = used;
        }
    }
}

fn lock(cache: &Mutex<Cache>) -> MutexGuard<'_, Cache> {
    // Critical sections cannot panic, so poisoning only means a panic elsewhere; the cache is
    // always consistent.
    cache.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Returns the entry for `key`, building it with `load` on a miss.
///
/// The cache lock is never held while `load` runs or while evicted values are dropped. Concurrent
/// misses on one key run exactly one `load` (the others wait on its result). If that load fails,
/// the waiters run their own `load` uncached so each gets a real error.
pub(crate) fn get_or_load(
    cache: &Mutex<Cache>,
    key: Key,
    load: impl FnOnce() -> Result<Prepared, Error>,
) -> Result<Prepared, Error> {
    // 1. Hit, or find/create the in-flight cell.
    let cell: Cell = {
        let mut guard = lock(cache);
        let Cache { slots, tick, .. } = &mut *guard;
        match slots.get_mut(&key) {
            Some(Slot::Ready {
                value, last_used, ..
            }) => {
                *last_used = *tick;
                *tick = tick.wrapping_add(1);
                return Ok(value.clone());
            }
            Some(Slot::Loading(cell)) => Arc::clone(cell),
            None => {
                let cell: Cell = Arc::new(OnceLock::new());
                slots.insert(key, Slot::Loading(Arc::clone(&cell)));
                cell
            }
        }
    };

    // 2. Without any lock: one thread runs `load`, the others block until it finishes.
    let mut load = Some(load);
    let mut own_err = None;
    let got: Option<Prepared> = cell
        .get_or_init(|| match load.take() {
            Some(f) => match f() {
                Ok(p) => Some(p),
                Err(e) => {
                    own_err = Some(e);
                    None
                }
            },
            None => None,
        })
        .clone();

    // 3. The first thread here removes the Loading slot and publishes the value.
    let mut evicted = Vec::new();
    {
        let mut guard = lock(cache);
        let ours = matches!(guard.slots.get(&key), Some(Slot::Loading(c)) if Arc::ptr_eq(c, &cell));
        if ours {
            guard.slots.remove(&key);
            if let Some(p) = &got {
                guard.insert(key, p, &mut evicted);
            }
        }
    }
    // Freeing a prepared dictionary can take time: outside the lock.
    drop(evicted);

    // 4. Result.
    match (got, own_err) {
        (Some(p), _) => Ok(p),
        (None, Some(e)) => Err(e),
        // We waited on a load that failed: run our own, uncached.
        (None, None) => match load {
            Some(f) => f(),
            // Unreachable: `load` is taken only by our own initializer, which sets `got` or
            // `own_err`. Fail without panicking anyway.
            None => Err(Error::NotFound {
                dict_id: key.dict_id(),
            }),
        },
    }
}
