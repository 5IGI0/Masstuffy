//! The read path shared by `get`, `get_raw`, the lookups and `scan`.

use std::sync::Arc;

use masstuffy_cdx::Entry;
use masstuffy_collection::{CollectionId, ReadCtx, ReadError, Snapshot};

use crate::collection::Shared;
use crate::error::Error;
use crate::repository::Core;
use crate::types::{Located, RawRecord};
use crate::writer::Published;

/// What to fetch with an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fetch {
    /// Nothing: locators only.
    #[default]
    None,
    /// The decoded, parsed and verified WARC record (as `get`).
    Decoded,
    /// The stored bytes and their encoding (as `get_raw`).
    Raw,
}

/// A fetched record.
#[derive(Debug)]
pub enum Fetched {
    /// The serialised WARC record (`Fetch::Decoded`).
    Decoded(Vec<u8>),
    /// The stored bytes (`Fetch::Raw`).
    Raw(RawRecord),
}

impl Fetched {
    /// Length in bytes of what was fetched.
    pub(crate) fn len(&self) -> u64 {
        let n = match self {
            Fetched::Decoded(v) => v.len(),
            Fetched::Raw(r) => r.bytes.len(),
        };
        u64::try_from(n).unwrap_or(u64::MAX)
    }
}

/// Snapshot of `shared` if `generation` is the active one, else `StaleLocator`.
pub(crate) fn snapshot_for(shared: &Shared, generation: u32) -> Result<Arc<Snapshot>, Error> {
    let snap = shared.published.load();
    if generation != snap.generation() {
        return Err(Error::StaleLocator {
            expected: snap.generation(),
            got: generation,
        });
    }
    Ok(snap)
}

/// Blocking: reads one record (`mode` must not be `Fetch::None`). Retries once with the current
/// snapshot on `SegmentMissing` (same generation, different `Arc`: a retired snapshot may have
/// lost its files). Errors are mapped: `EntryMismatch | Record` with `Decoded` give
/// `NotDecodable`, the others `Read { collection }`.
pub(crate) fn read_blocking(
    snap: &Arc<Snapshot>,
    published: &Published,
    collection: CollectionId,
    entry: &Entry<'_>,
    mode: Fetch,
    ctx: &mut ReadCtx,
) -> Result<Fetched, Error> {
    let mut out = Vec::new();
    let res = match mode {
        Fetch::None => return Err(Error::Internal("fetch mode")),
        Fetch::Decoded => {
            let mut r = snap.read(entry, ctx, &mut out).map(|_| ());
            if matches!(r, Err(ReadError::SegmentMissing { .. })) {
                let cur = published.load();
                if !Arc::ptr_eq(snap, &cur) && cur.generation() == snap.generation() {
                    out.clear();
                    r = cur.read(entry, ctx, &mut out).map(|_| ());
                }
            }
            r.map(|()| Fetched::Decoded(out))
        }
        Fetch::Raw => {
            let mut r = snap.read_raw(entry, &mut out);
            if matches!(r, Err(ReadError::SegmentMissing { .. })) {
                let cur = published.load();
                if !Arc::ptr_eq(snap, &cur) && cur.generation() == snap.generation() {
                    out.clear();
                    r = cur.read_raw(entry, &mut out);
                }
            }
            r.map(|encoding| {
                Fetched::Raw(RawRecord {
                    bytes: out,
                    encoding,
                })
            })
        }
    };
    res.map_err(|source| match source {
        ReadError::EntryMismatch(_) | ReadError::Record(_) if mode == Fetch::Decoded => {
            Error::NotDecodable { source }
        }
        source => Error::Read { collection, source },
    })
}

/// Async: checks the locator's generation, takes a read permit, then reads in `spawn_blocking`
/// with a pooled `ReadCtx`.
pub(crate) async fn read_located(
    core: &Arc<Core>,
    shared: &Arc<Shared>,
    at: &Located,
    mode: Fetch,
) -> Result<Fetched, Error> {
    if mode == Fetch::None {
        return Err(Error::Internal("fetch mode"));
    }
    let snap = snapshot_for(shared, at.generation)?;
    let permit = Arc::clone(&core.read_sem)
        .acquire_owned()
        .await
        .map_err(|_| Error::ShuttingDown)?;
    let core2 = Arc::clone(core);
    let published = Arc::clone(&shared.published);
    let entry = at.entry.clone();
    let collection = at.collection;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut ctx = core2.read_ctxs.take();
        let r = read_blocking(&snap, &published, collection, &entry, mode, &mut ctx);
        core2.read_ctxs.give(ctx);
        r
    })
    .await
    .map_err(Error::Join)?
}
