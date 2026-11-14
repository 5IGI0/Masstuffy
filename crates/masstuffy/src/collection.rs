//! One loaded collection: the shared state, the public handle, and the put paths.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use masstuffy_collection::{CollectionId, PrepareError, Prepared, Preparer, RawCheck};
use masstuffy_warc::{Record, RecordBuilder};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::error::Error;
use crate::pool::PreparerPool;
use crate::repository::Core;
use crate::types::{CollectionInfo, Located, RawItem, WriterStats};
use crate::writer::{Committed, Msg, Published, Rejected, Reply, Request, WriteError};

/// One per collection; holds the only long-lived `mpsc::Sender` of its writer task.
pub(crate) struct Shared {
    pub(crate) id: CollectionId,
    /// Current slug. Std RwLock, leaf: written only by the rename unit while it holds the
    /// registry write lock; read with a clone of the `Arc`.
    pub(crate) slug: RwLock<Arc<str>>,
    pub(crate) tx: mpsc::Sender<Msg>,
    pub(crate) published: Arc<Published>,
    /// What the secondary index tails for this collection.
    pub(crate) feed: Arc<crate::indexer::IndexFeed>,
    pub(crate) preparers: PreparerPool,
    /// Per-collection admin mutex (architecture 4.7): rebuild, delete_collection, rename, record
    /// deletion, `rebuild_index`, shutdown; not training. `Arc` so that callers can take an owned
    /// guard before spawning a unit (`lock_owned`).
    pub(crate) admin: Arc<tokio::sync::Mutex<()>>,
    /// Per-collection admission gate. Puts read-hold it (after `Core::gate`) from admission until
    /// their request is queued; `delete_collection` write-holds it only to set `deleted`.
    pub(crate) admission: tokio::sync::RwLock<()>,
    /// Set once (never reset) by `delete_collection` under `admission`'s write guard, before it
    /// sets the rebuild cancel flag. Every handle operation fails with `UnknownCollection` after.
    pub(crate) deleted: AtomicBool,
    /// Rebuild state shared with the writer task.
    pub(crate) rebuild: Arc<crate::rebuild::RebuildSlot>,
    pub(crate) task: Mutex<Option<JoinHandle<()>>>,
    #[cfg(test)]
    pub(crate) hooks: Arc<crate::writer::TestHooks>,
}

impl Shared {
    /// The current slug (read lock, `Arc` clone).
    pub(crate) fn slug(&self) -> Arc<str> {
        Arc::clone(&self.slug.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Replaces the slug. Called by the rename unit under the registry write lock (leaf lock).
    pub(crate) fn set_slug(&self, s: Arc<str>) {
        *self.slug.write().unwrap_or_else(PoisonError::into_inner) = s;
    }

    /// True once `delete_collection` has stopped admission.
    pub(crate) fn is_deleted(&self) -> bool {
        self.deleted.load(Ordering::Acquire)
    }

    /// `Err(UnknownCollection)` once the collection is deleted.
    pub(crate) fn check_live(&self) -> Result<(), Error> {
        if self.is_deleted() {
            Err(Error::UnknownCollection(self.id.to_string()))
        } else {
            Ok(())
        }
    }
}

/// Times a put is prepared again after a rebuild switched generations under it.
const MAX_STALE_RETRIES: u32 = 3;

/// The result of a queued request.
pub(crate) enum Outcome {
    Done(Vec<Located>),
    /// Prepared for a generation that is no longer the writer's: nothing was written.
    Stale(Vec<Prepared>),
}

/// A handle on one collection. Cheap to clone; usable from any task.
///
/// Holding a handle keeps the collection's writer task alive; call
/// [`Repository::shutdown`](crate::Repository::shutdown) for an orderly stop.
#[derive(Clone)]
pub struct CollectionHandle {
    pub(crate) core: Arc<Core>,
    pub(crate) shared: Arc<Shared>,
}

/// Input permits (1 KiB each) for `total` bytes: at least 1, at most the whole budget, so a
/// request larger than the budget runs alone.
fn admission_permits(total: u64, max: u32) -> u32 {
    let kib = total.div_ceil(1024).max(1);
    u32::try_from(kib.min(u64::from(max))).unwrap_or(max)
}

fn stored_total(items: &[Prepared]) -> u64 {
    items
        .iter()
        .fold(0u64, |a, p| a.saturating_add(p.stored_len()))
}

fn len_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

impl CollectionHandle {
    /// The collection uuid.
    #[must_use]
    pub fn id(&self) -> CollectionId {
        self.shared.id
    }

    /// The slug.
    #[must_use]
    pub fn slug(&self) -> Arc<str> {
        self.shared.slug()
    }

    /// Summary of the collection (codec and generation of the current snapshot).
    #[must_use]
    pub fn info(&self) -> CollectionInfo {
        let snap = self.shared.published.load();
        CollectionInfo {
            id: self.shared.id,
            slug: self.shared.slug(),
            codec: snap.manifest().active().codec,
            generation: snap.generation(),
        }
    }

    /// Counters of the writer task.
    #[must_use]
    pub fn stats(&self) -> WriterStats {
        let p = &self.shared.published;
        WriterStats {
            commits: p.commits.load(Ordering::Acquire),
            records: p.records.load(Ordering::Acquire),
            cdx_len: p.cdx_len.load(Ordering::Acquire),
        }
    }

    /// Appends `records` (spec 04 4.2a and 4.2c). A missing `WARC-Record-ID` or `WARC-Date` is
    /// generated (spec 03.1). Returns one [`Located`] per record, in order; the records are
    /// contiguous in one segment and consecutive in the index (I9). Empty input returns an empty
    /// vector without any I/O.
    ///
    /// A request prepared for the generation that a rebuild replaces is re-encoded for the new
    /// generation (raw frames are decoded and re-encoded with the new dictionary, as the rebuild
    /// does) and committed there.
    ///
    /// Concurrent puts to one collection are merged into group commits (one fsync pair per group);
    /// the call returns only after its records are durable.
    ///
    /// # Cancellation
    ///
    /// Dropping the future before the request is queued writes nothing. After that the request is
    /// committed (or fails) regardless and the reply is discarded: a retried put may duplicate
    /// records (at-least-once, duplicate `record_id` is allowed).
    ///
    /// # Errors
    ///
    /// `ShuttingDown`, `Build` / `Prepare` (nothing written), `StaleGeneration` (nothing written),
    /// `Commit`, `Reopen`, `CollectionUnavailable` (the writer stopped before replying: the request may or may not be
    /// durable), `Join`.
    pub async fn put(&self, records: Vec<Record>) -> Result<Vec<Located>, Error> {
        let sizes = records
            .iter()
            .fold(0u64, |a, r| a.saturating_add(len_u64(r.as_bytes().len())));
        self.put_with(records, sizes, prepare_records).await
    }

    /// Appends already stored records (spec 04 4.2b and 4.2c), checked with the repository's
    /// [`RawCheck`]. Same contract, ordering, cancellation and errors as [`put`](Self::put); no
    /// header is filled in.
    ///
    /// # Errors
    ///
    /// As [`put`](Self::put), with `Prepare` for any rejected item. A raw frame prepared for the
    /// generation that a rebuild replaces is decoded and re-encoded for the new one (as the
    /// rebuild does) and committed there.
    pub async fn put_raw(&self, items: Vec<RawItem>) -> Result<Vec<Located>, Error> {
        let sizes = items
            .iter()
            .fold(0u64, |a, r| a.saturating_add(len_u64(r.payload.len())));
        let check = self.core.raw_check;
        self.put_with(items, sizes, move |p, items| {
            prepare_raw_items(p, items, check)
        })
        .await
    }

    async fn put_with<T, F>(
        &self,
        input: Vec<T>,
        input_bytes: u64,
        prepare: F,
    ) -> Result<Vec<Located>, Error>
    where
        T: Send + 'static,
        F: FnOnce(&mut Preparer, Vec<T>) -> Result<Vec<Prepared>, Error> + Send + 'static,
    {
        if input.is_empty() {
            return Ok(Vec::new());
        }
        self.put_stage(input, input_bytes, move |p, v| {
            prepare(p, v).map(|v| (v, ()))
        })
        .await
        .map(|(v, ())| v)
    }

    /// Admission (gate read guard, inflight permits, prepare permit), prepare in the blocking
    /// pool, submit, wait. If `prepare` returns no items, nothing is submitted and
    /// `Ok((vec![], x))` is returned (the gate and permits are released).
    pub(crate) async fn put_stage<T, X, F>(
        &self,
        input: Vec<T>,
        input_bytes: u64,
        prepare: F,
    ) -> Result<(Vec<Located>, X), Error>
    where
        T: Send + 'static,
        X: Send + 'static,
        F: FnOnce(&mut Preparer, Vec<T>) -> Result<(Vec<Prepared>, X), Error> + Send + 'static,
    {
        let core = &self.core;
        // Held until the request is queued, on purpose: shutdown flips the flag under the write
        // guard, so every admitted put has queued its request before the writers are stopped.
        let gate = core.gate.read().await;
        if *gate {
            return Err(Error::ShuttingDown);
        }
        // Held with `gate` until the request is queued: `delete_collection` write-holds it to set
        // `deleted`, so every put admitted before that has queued its request before the writer
        // is stopped, and every later one fails.
        let adm = self.shared.admission.read().await;
        self.shared.check_live()?;
        let kib = admission_permits(input_bytes, core.limits.inflight_kib);
        let permit = Arc::clone(&core.inflight_sem)
            .acquire_many_owned(kib)
            .await
            .map_err(|_| Error::ShuttingDown)?;
        let cpu = Arc::clone(&core.prepare_sem)
            .acquire_owned()
            .await
            .map_err(|_| Error::ShuttingDown)?;
        let ctx = Arc::clone(self.shared.published.load().context());
        let shared = Arc::clone(&self.shared);
        let (items, extra) = tokio::task::spawn_blocking(move || {
            let _cpu = cpu;
            let mut p = shared
                .preparers
                .take(&ctx)
                .map_err(|source| Error::Prepare { index: 0, source })?;
            let r = prepare(&mut p, input);
            shared.preparers.give(p);
            r
        })
        .await
        .map_err(Error::Join)??;
        if items.is_empty() {
            return Ok((Vec::new(), extra));
        }
        #[cfg(test)]
        self.shared.hooks.wait_after_prepare().await;
        let bytes = stored_total(&items);
        let rx = self.submit(items, bytes, permit).await?;
        drop(adm);
        drop(gate);
        let mut outcome = self.await_reply(rx).await?;
        let mut retries = 0;
        loop {
            match outcome {
                Outcome::Done(located) => return Ok((located, extra)),
                Outcome::Stale(items) => {
                    if retries >= MAX_STALE_RETRIES {
                        return Err(Error::StaleGeneration(self.shared.id));
                    }
                    retries += 1;
                    outcome = self.resubmit(items, kib).await?;
                }
            }
        }
    }

    /// Prepares `items` again for the current generation (a rebuild switched while they were
    /// queued) and queues them. Nothing of them was written.
    async fn resubmit(&self, items: Vec<Prepared>, kib: u32) -> Result<Outcome, Error> {
        let core = &self.core;
        let gate = core.gate.read().await;
        if *gate {
            return Err(Error::ShuttingDown);
        }
        let adm = self.shared.admission.read().await;
        self.shared.check_live()?;
        let permit = Arc::clone(&core.inflight_sem)
            .acquire_many_owned(kib)
            .await
            .map_err(|_| Error::ShuttingDown)?;
        let cpu = Arc::clone(&core.prepare_sem)
            .acquire_owned()
            .await
            .map_err(|_| Error::ShuttingDown)?;
        let shared = Arc::clone(&self.shared);
        let items = tokio::task::spawn_blocking(move || {
            let _cpu = cpu;
            let ctx = Arc::clone(shared.published.load().context());
            let mut p = shared
                .preparers
                .take(&ctx)
                .map_err(|source| Error::Prepare { index: 0, source })?;
            let r = items
                .into_iter()
                .enumerate()
                .map(|(index, x)| {
                    p.reprepare(x)
                        .map_err(|source| Error::Prepare { index, source })
                })
                .collect::<Result<Vec<_>, _>>();
            shared.preparers.give(p);
            r
        })
        .await
        .map_err(Error::Join)??;
        let bytes = stored_total(&items);
        let rx = self.submit(items, bytes, permit).await?;
        drop(adm);
        drop(gate);
        self.await_reply(rx).await
    }

    /// Queues a prepared request. Nothing is written if this fails.
    pub(crate) async fn submit(
        &self,
        items: Vec<Prepared>,
        bytes: u64,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<oneshot::Receiver<Reply>, Error> {
        let (reply, rx) = oneshot::channel();
        self.shared
            .tx
            .send(Msg::Commit(Request {
                items,
                bytes,
                reply,
                permit,
            }))
            .await
            .map_err(|_| Error::CollectionUnavailable(self.shared.id))?;
        Ok(rx)
    }

    /// Waits for the reply of a queued request and maps it.
    pub(crate) async fn await_reply(&self, rx: oneshot::Receiver<Reply>) -> Result<Outcome, Error> {
        let id = self.shared.id;
        match rx.await {
            Err(_) => Err(Error::CollectionUnavailable(id)),
            Ok(Err(Rejected::Stale(items))) => Ok(Outcome::Stale(items)),
            Ok(Err(Rejected::Error(w))) => Err(match w {
                WriteError::Commit(source) => Error::Commit {
                    collection: id,
                    source,
                },
                WriteError::Reopen(source) => Error::Reopen {
                    collection: id,
                    source,
                },
                WriteError::Internal(s) => Error::Internal(s),
            }),
            Ok(Ok(Committed {
                generation,
                entries,
            })) => Ok(Outcome::Done(
                entries
                    .into_iter()
                    .map(|entry| Located {
                        collection: id,
                        generation,
                        entry,
                    })
                    .collect(),
            )),
        }
    }

    /// Test helper: `await_reply` with `Stale` mapped to `StaleGeneration`.
    #[cfg(test)]
    pub(crate) async fn finish(&self, rx: oneshot::Receiver<Reply>) -> Result<Vec<Located>, Error> {
        match self.await_reply(rx).await? {
            Outcome::Done(l) => Ok(l),
            Outcome::Stale(_) => Err(Error::StaleGeneration(self.shared.id)),
        }
    }

    /// Test helper: prepares `records` and queues them without waiting for the reply.
    #[cfg(test)]
    pub(crate) async fn submit_records(&self, records: Vec<Record>) -> oneshot::Receiver<Reply> {
        let kib = admission_permits(1, self.core.limits.inflight_kib);
        let permit = Arc::clone(&self.core.inflight_sem)
            .acquire_many_owned(kib)
            .await
            .unwrap();
        let ctx = Arc::clone(self.shared.published.load().context());
        let mut p = self.shared.preparers.take(&ctx).unwrap();
        let items = prepare_records(&mut p, records).unwrap();
        self.shared.preparers.give(p);
        let bytes = items.iter().map(Prepared::stored_len).sum();
        self.submit(items, bytes, permit).await.unwrap()
    }
}

/// Fills missing `WARC-Record-ID` / `WARC-Date`, then serialises and compresses. Stops at the
/// first error.
fn prepare_records(p: &mut Preparer, records: Vec<Record>) -> Result<Vec<Prepared>, Error> {
    let mut out = Vec::with_capacity(records.len());
    for (index, record) in records.into_iter().enumerate() {
        let needs_fill = {
            let v = record.view();
            v.record_id().is_none() || v.date().is_none()
        };
        let record = if needs_fill {
            let v = record.view();
            let mut b = RecordBuilder::from_record_ref(&v);
            b.fill_missing()
                .map_err(|source| Error::Build { index, source })?;
            b.build(v.body())
                .map_err(|source| Error::Build { index, source })?
        } else {
            record
        };
        out.push(
            p.prepare(record)
                .map_err(|source: PrepareError| Error::Prepare { index, source })?,
        );
    }
    Ok(out)
}

fn prepare_raw_items(
    p: &mut Preparer,
    items: Vec<RawItem>,
    check: RawCheck,
) -> Result<Vec<Prepared>, Error> {
    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        out.push(
            p.prepare_raw(&item.meta, item.payload, check)
                .map_err(|source| Error::Prepare { index, source })?,
        );
    }
    Ok(out)
}
