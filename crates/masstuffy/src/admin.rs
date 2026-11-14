//! Administrative operations: `delete_collection`, `rename_collection` and record deletion.
//!
//! None of the three takes `Core::gate` (a long rebuild holds an `admin` mutex, and waiting for it
//! under the gate read guard would block shutdown's `gate.write()` for the whole rebuild). Each
//! one checks `Core::is_closing` **after** acquiring the collection's `admin` mutex and returns
//! `ShuttingDown` if shutdown has started: shutdown sets `closing` right after the gate flip and
//! holds every `admin` it took until it ends, so a flow that got `admin` first finishes with the
//! writer task alive.
//!
//! # `delete_collection` (spec 04 4.6, spec 05 5.3 [RECO])
//!
//! Everything runs in one spawned unit, so dropping the caller cannot leave a half-done delete:
//!
//! 1. write-lock `Shared::admission` only to set `deleted` (every put admitted earlier has queued
//!    its request; later puts and handle operations fail with `UnknownCollection`);
//! 2. set the rebuild cancel flag (after `deleted`: `start_job` resets it, then reads `deleted`);
//! 3. take `admin` (waits for a rebuild, which stops at its next check, `rebuild_index`, a rename,
//!    a record delete or another delete);
//! 4. stop the writer task (`Msg::Shutdown`): it commits what was queued, writes its final
//!    checkpoint and exits;
//! 5. **commit point**: `durable::trash_dir` (rename to `.trash-*`, fsync of the parent, then
//!    removal);
//! 6. registry: the id and the slug are removed (the slug is kept reserved if the parent fsync
//!    failed: a crash could bring the directory back);
//! 7. `IndexCmd::Forget`: hub and track removal, then `DropCollection`.
//!
//! | Crash after | On disk | Next open |
//! |---|---|---|
//! | steps 1-4 (before the rename) | collection intact (final checkpoint maybe written) | loaded normally; the delete did not happen |
//! | rename, before the parent fsync is durable | intact **or** `.trash-<uuid>-<pid>-<seq>` | intact: loaded; trash: as below |
//! | rename durable, removal partial or not started | `.trash-*` leftover | `sweep_trash` removes it; the index rows are dropped by the orphan GC of the first pass |
//! | removal done, before `Forget` | nothing | index rows dropped by the orphan GC at open |
//! | `Forget` applied | nothing | nothing to do |
//!
//! In-flight work: puts admitted before step 1 are committed and acknowledged; later ones get
//! `UnknownCollection`. A reader that already holds a snapshot may still succeed (cached file
//! descriptors stay valid on Unix after the rename and unlink) or fail with `Read`. A training in
//! progress fails with `Read` or `StaleLocator`. A rebuild is cancelled and returns
//! `UnknownCollection`. A record delete or rename waiting for `admin` gets `UnknownCollection`.
//!
//! # `rename_collection` (spec 02 "Répertoire")
//!
//! The guards (`admin`, then `create_lock`) are taken in the caller (cancellable, nothing
//! mutated), then a spawned unit owning them asks the writer task to rewrite the manifest slug
//! (`Msg::SetSlug`): `atomic_write` of the manifest is the commit point. The registry follows
//! only after it, and the old slug is reserved when the outcome is ambiguous.
//!
//! # Record deletion (spec 04 4.6 "Un record" [RECO])
//!
//! `CollectionHandle::delete` takes `admin` (owned) in the caller, then a spawned unit prepares
//! the tombstones and queues `Msg::Tombstone`; the writer task appends the `T` lines (one CDX
//! append and fsync). The unit owns `admin` until the writer replied, so no rebuild can start
//! between the queuing and the fsync.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::PoisonError;

use masstuffy_collection::CollectionId;
use masstuffy_durable::Op;
use masstuffy_index::CollectionKey;
use tokio::sync::oneshot;
use tracing::warn;

use crate::collection::{CollectionHandle, Shared};
use crate::error::Error;
use crate::indexer::IndexCmd;
use crate::query::{check_key, located_of, lookup_id, read_permit};
use crate::repository::{Inner, Repository, collection_dir, index_request_with};
use crate::types::{CollectionRef, DeletedCollection, Located, MAX_DELETE_TARGETS};
use crate::writer::{
    DeletePoint, Msg, SlugApplied, SlugFail, SlugRequest, TombstoneRequest, WriteError,
};

/// Test hook: records that `p` was reached, then "crashes" there if a test asked: the unit
/// returns without any cleanup.
#[cfg(test)]
fn delete_point(shared: &Shared, p: DeletePoint) -> Result<(), Error> {
    let h = &shared.hooks;
    h.delete_reached.send_replace(Some(p));
    let crash = *h
        .delete_crash
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if crash == Some(p) {
        return Err(Error::Internal("injected delete crash"));
    }
    Ok(())
}

#[cfg(not(test))]
#[allow(clippy::unnecessary_wraps)] // Same signature as the test hook.
fn delete_point(_shared: &Shared, _p: DeletePoint) -> Result<(), Error> {
    Ok(())
}

impl Repository {
    /// Deletes a loaded collection (spec 04 4.6 "Une collection", spec 05 5.3 \[RECO\]).
    ///
    /// Protocol (see the module docs): admission is closed (later puts and every other handle
    /// operation fail with `UnknownCollection`), a running rebuild is cancelled, the writer task
    /// commits what was queued and exits, then the directory is moved to `.trash-*` (the commit
    /// point: rename, then fsync of the parent) and removed. The slug is free afterwards (kept
    /// reserved if the parent fsync failed), and the index rows are dropped. A crash at any point
    /// is repaired by `Repository::open` (trash sweep, orphan GC of the index).
    ///
    /// Puts admitted before the call are committed and acknowledged. A reader that already holds
    /// a snapshot may still succeed (cached file descriptors stay valid after the rename and
    /// unlink) or fail with `Read` once the files are gone. A collection skipped at open is not
    /// loaded and cannot be deleted through the API (`UnknownCollection`).
    ///
    /// The delete runs in a spawned unit: dropping this future does not stop it.
    ///
    /// # Errors
    ///
    /// `ShuttingDown`, `UnknownCollection` (not loaded, or already deleted), `Durable` (the trash
    /// rename failed: nothing was deleted; the collection stays registered but closed: call
    /// again, or reopen the repository to use it), `Internal`, `Join`.
    pub async fn delete_collection<'a>(
        &self,
        which: impl Into<CollectionRef<'a>>,
    ) -> Result<DeletedCollection, Error> {
        self.check_open()?;
        let shared = self.collection_ref(which.into())?;
        tokio::spawn(delete_unit(Arc::clone(&self.inner), shared))
            .await
            .map_err(Error::Join)?
    }

    /// Renames a loaded collection (spec 02 "Répertoire": manifest only, the directory keeps its
    /// uuid name). `new_slug` must match `[a-z0-9][a-z0-9_-]{0,127}` and be unused (also by a
    /// skipped collection). Renaming to the current slug is a no-op. Waits for a running
    /// rebuild / `rebuild_index` / record delete of that collection (admin mutex); the wait is
    /// cancellable, and nothing has changed until it ends. Returns a handle (its `slug()` is the
    /// new one; existing handles see the new slug too).
    ///
    /// # Errors
    ///
    /// `ShuttingDown`, `InvalidSlug`, `UnknownCollection`, `SlugTaken`, `Collection` (the
    /// manifest write failed: the registry follows the namespace, and a slug that a crash could
    /// bring back stays reserved), `CollectionUnavailable`, `Join`.
    pub async fn rename_collection<'a>(
        &self,
        which: impl Into<CollectionRef<'a>>,
        new_slug: &str,
    ) -> Result<CollectionHandle, Error> {
        self.check_open()?;
        if !crate::layout::is_valid_new_slug(new_slug) {
            return Err(Error::InvalidSlug {
                slug: new_slug.to_owned(),
            });
        }
        let shared = self.collection_ref(which.into())?;
        shared.check_live()?;
        // Cancellable waits: nothing is mutated before the unit starts.
        let admin = Arc::clone(&shared.admin).lock_owned().await;
        if self.inner.core.is_closing() {
            return Err(Error::ShuttingDown);
        }
        shared.check_live()?;
        let create = Arc::clone(&self.inner.create_lock).lock_owned().await;
        tokio::spawn(rename_unit(
            Arc::clone(&self.inner),
            Arc::clone(&shared),
            Arc::from(new_slug),
            admin,
            create,
        ))
        .await
        .map_err(Error::Join)??;
        Ok(self.handle(shared))
    }
}

async fn delete_unit(inner: Arc<Inner>, shared: Arc<Shared>) -> Result<DeletedCollection, Error> {
    let core = &inner.core;
    let id = shared.id;
    // 1. Stop admission. Once the write guard is acquired, every put admitted earlier has queued
    // its request.
    {
        let _w = shared.admission.write().await;
        shared
            .deleted
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    // 2. Abort a rebuild (after `deleted`: `start_job` resets `cancel`, then reads `deleted`).
    shared
        .rebuild
        .cancel
        .store(true, std::sync::atomic::Ordering::SeqCst);
    // 3. Held to the end, on purpose: exclusivity with rebuilds, renames, record deletes,
    // `rebuild_index` and shutdown.
    let _admin = shared.admin.lock().await;
    if core.is_closing() {
        return Err(Error::ShuttingDown);
    }
    // 5. Already deleted by a concurrent call?
    {
        let reg = inner.registry();
        if !matches!(reg.by_id.get(&id), Some(s) if Arc::ptr_eq(s, &shared)) {
            return Err(Error::UnknownCollection(id.to_string()));
        }
    }
    // 6. Stop the writer task: it commits what was queued, writes its final checkpoint and exits.
    let handle = shared
        .task
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some(h) = handle {
        let (ack_tx, ack_rx) = oneshot::channel();
        if shared.tx.send(Msg::Shutdown(ack_tx)).await.is_ok() {
            let _ = ack_rx.await; // Err: the task ended without acknowledging.
        }
        if let Err(e) = h.await {
            warn!(collection = %id, "writer task of a deleted collection failed: {e}");
        }
    }
    // (else: an earlier failed attempt already stopped it.)
    delete_point(&shared, DeletePoint::Stopped)?;

    // 8. Trash: the commit point.
    let slug = shared.slug();
    let path = collection_dir(&core.dir, id);
    let lock = core.lock_arc().ok_or(Error::ShuttingDown)?;
    let slot = Arc::clone(&shared.rebuild);
    let trashed = tokio::task::spawn_blocking(move || {
        let _lock = lock;
        // Defensive: the slot is always empty here (the rebuild unit ended, `admin` is ours).
        drop(
            slot.rb
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        masstuffy_durable::trash_dir(&path)
    })
    .await
    .map_err(Error::Join)?;
    let (trash_removed, slug_reserved) = match trashed {
        Ok(()) => (true, false),
        Err(e) if e.op() == Some(Op::Stat) && e.io_kind() == Some(std::io::ErrorKind::NotFound) => {
            warn!(collection = %id, "collection directory was already gone: {e}");
            (true, false)
        }
        Err(e) if e.op() == Some(Op::SyncDir) => {
            warn!(collection = %id, "trash rename not known to be durable: {e}");
            (false, true)
        }
        Err(e) if e.op() == Some(Op::RemoveTrash) => {
            warn!(collection = %id, "cannot remove the trash directory (the next open will): {e}");
            (false, false)
        }
        // Not committed: the writer is stopped and `deleted` is set, the collection stays
        // registered. A later call retries from step 1.
        Err(e) => return Err(Error::Durable(e)),
    };
    delete_point(&shared, DeletePoint::Trashed)?;

    // 10. Registry: the slug is freed only now.
    {
        let mut reg = inner.registry_mut();
        reg.by_id.remove(&id);
        if reg.by_slug.get(&*slug) == Some(&id) {
            reg.by_slug.remove(&*slug);
        }
        if slug_reserved {
            reg.reserved.insert(slug.to_string());
        }
    }
    // 11. Index. Not raced with `closing`: shutdown cannot reach the indexer while this unit
    // holds `admin`. The writer task has been joined, so nothing can wake the feed again.
    let key = CollectionKey::new(id.as_u128());
    let index_dropped =
        match index_request_with(core, |reply| IndexCmd::Forget { key, reply }).await {
            Ok(()) => true,
            Err(e) => {
                warn!(collection = %id, "index rows not dropped (the orphan GC will): {e}");
                false
            }
        };
    Ok(DeletedCollection {
        id,
        slug,
        trash_removed,
        slug_reserved,
        index_dropped,
    })
}

/// Makes the registry and the shared slug follow a manifest that now holds `new`.
fn apply_rename(inner: &Inner, shared: &Shared, id: CollectionId, old: &Arc<str>, new: &Arc<str>) {
    let mut reg = inner.registry_mut();
    if reg.by_slug.get(&**old) == Some(&id) {
        reg.by_slug.remove(&**old);
    }
    reg.by_slug.insert(Arc::clone(new), id);
    // Leaf lock, inside the registry write section: a lookup never sees both slugs or neither.
    shared.set_slug(Arc::clone(new));
}

async fn rename_unit(
    inner: Arc<Inner>,
    shared: Arc<Shared>,
    new: Arc<str>,
    _admin: tokio::sync::OwnedMutexGuard<()>,
    _create: tokio::sync::OwnedMutexGuard<()>,
) -> Result<(), Error> {
    let id = shared.id;
    let old = shared.slug();
    {
        let reg = inner.registry();
        if *old == *new {
            return Ok(());
        }
        if !matches!(reg.by_id.get(&id), Some(s) if Arc::ptr_eq(s, &shared)) {
            return Err(Error::UnknownCollection(id.to_string()));
        }
        // `create_lock` is ours: no create or rename can take `new` until this unit ends.
        if reg.by_slug.contains_key(&*new) || reg.reserved.contains(&*new) {
            return Err(Error::SlugTaken {
                slug: new.to_string(),
            });
        }
    }
    let (tx, rx) = oneshot::channel();
    shared
        .tx
        .send(Msg::SetSlug(SlugRequest {
            slug: Arc::clone(&new),
            reply: tx,
        }))
        .await
        .map_err(|_| Error::CollectionUnavailable(id))?;
    let reply = rx.await.map_err(|_| Error::CollectionUnavailable(id))?;
    match reply {
        Ok(()) => {
            apply_rename(&inner, &shared, id, &old, &new);
            Ok(())
        }
        Err(SlugFail { source, applied }) => {
            match applied {
                SlugApplied::Yes => {
                    // The manifest may revert after a crash: the old slug stays reserved.
                    apply_rename(&inner, &shared, id, &old, &new);
                    inner.registry_mut().reserved.insert(old.to_string());
                }
                SlugApplied::No => {}
                SlugApplied::Unknown => {
                    inner.registry_mut().reserved.insert(new.to_string());
                }
            }
            Err(Error::Collection {
                collection: id,
                source,
            })
        }
    }
}

impl CollectionHandle {
    /// Logically deletes records (spec 04 4.6 "Un record" \[RECO\]): one `T` line per distinct
    /// target, appended to the CDX and fsynced before this returns. Scans and index-backed
    /// queries (after `sync_index`) no longer return them; the next rebuild drops them. A locator
    /// read with `get`/`get_raw` still returns the bytes until that rebuild (logical deletion, no
    /// zeroing). Deleting an already deleted record succeeds (another harmless `T` line).
    /// Duplicate targets in one call are written once. All-or-nothing validation: any invalid
    /// target fails the call with nothing written.
    ///
    /// Serialised with rebuilds, renames, `rebuild_index` and other deletes of this collection
    /// (admin mutex): **during a rebuild the call waits** for it to end, and a target of the
    /// replaced generation then fails with `StaleLocator` (find it again, e.g. `delete_by_id`
    /// after `sync_index`). The wait is cancellable (nothing written); once the tombstones are
    /// queued they are written even if the future is dropped.
    ///
    /// Returns the number of `T` lines written. Empty input returns 0 without I/O.
    ///
    /// # Errors
    ///
    /// `InvalidOptions` (more than `MAX_DELETE_TARGETS`, or a locator of another collection),
    /// `UnknownCollection` (deleted), `ShuttingDown`, `StaleLocator`, `Tombstone { index, source }`
    /// (warcinfo, not an `Add`, past the segment end, missing segment), `Commit` (incl.
    /// `TombstoneNotCommitted`: the locator was never acknowledged), `Reopen`,
    /// `CollectionUnavailable`, `Join`.
    pub async fn delete(&self, targets: Vec<Located>) -> Result<usize, Error> {
        if targets.is_empty() {
            return Ok(0);
        }
        if targets.len() > MAX_DELETE_TARGETS {
            return Err(Error::InvalidOptions("too many delete targets"));
        }
        if targets.iter().any(|t| t.collection != self.shared.id) {
            return Err(Error::InvalidOptions("locator of another collection"));
        }
        // Dedupe by locator, keeping the first occurrence and the order.
        let mut seen: HashSet<(u32, u64)> = HashSet::with_capacity(targets.len());
        let mut targets = targets;
        targets.retain(|t| seen.insert((t.entry.segment, t.entry.offset)));
        self.shared.check_live()?;
        if self.core.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Error::ShuttingDown);
        }
        // Cancellable wait: nothing is written before the unit starts.
        let admin = Arc::clone(&self.shared.admin).lock_owned().await;
        if self.core.is_closing() {
            return Err(Error::ShuttingDown);
        }
        self.shared.check_live()?;
        tokio::spawn(delete_records_unit(
            Arc::clone(&self.shared),
            targets,
            admin,
        ))
        .await
        .map_err(Error::Join)?
    }

    /// Index-backed: deletes the most recently committed visible copy of `record_id` in this
    /// collection and returns its locator, or `None` if the index has none. Eventually consistent
    /// (call `Repository::sync_index` first for read-your-writes); with duplicate ids only one
    /// copy is deleted per call (after `sync_index` the next call finds the next copy). A hit of
    /// another generation (index lagging a rebuild) gives `StaleLocator`.
    ///
    /// # Errors
    ///
    /// `InvalidQuery`, `IndexQuery`, plus the errors of [`delete`](Self::delete).
    pub async fn delete_by_id(&self, record_id: &str) -> Result<Option<Located>, Error> {
        check_key(record_id, "record_id")?;
        self.shared.check_live()?;
        let permit = read_permit(&self.core).await?;
        let (hit, permit) = lookup_id(&self.core, record_id, Some(self.shared.id), permit).await?;
        drop(permit);
        let Some(hit) = hit else {
            return Ok(None);
        };
        let located = located_of(hit);
        self.delete(vec![located.clone()]).await?;
        Ok(Some(located))
    }
}

/// Prepares and appends the tombstones. Owns `admin` until the writer replied, so no rebuild can
/// start between the queuing and the fsync of the `T` lines.
async fn delete_records_unit(
    shared: Arc<Shared>,
    targets: Vec<Located>,
    _admin: tokio::sync::OwnedMutexGuard<()>,
) -> Result<usize, Error> {
    let id = shared.id;
    let snap = shared.published.load();
    // Tombstones are written to the current generation only (the retired snapshot is refused).
    for t in &targets {
        if t.generation != snap.generation() {
            return Err(Error::StaleLocator {
                expected: snap.generation(),
                got: t.generation,
            });
        }
    }
    // `prepare_tombstone` does an `fstat`: not on the runtime.
    let items = tokio::task::spawn_blocking(move || {
        targets
            .iter()
            .enumerate()
            .map(|(index, t)| {
                snap.prepare_tombstone(&t.entry)
                    .map_err(|source| Error::Tombstone { index, source })
            })
            .collect::<Result<Vec<_>, Error>>()
    })
    .await
    .map_err(Error::Join)??;
    let n = items.len();
    let (tx, rx) = oneshot::channel();
    shared
        .tx
        .send(Msg::Tombstone(TombstoneRequest { items, reply: tx }))
        .await
        .map_err(|_| Error::CollectionUnavailable(id))?;
    match rx.await.map_err(|_| Error::CollectionUnavailable(id))? {
        Ok(()) => Ok(n),
        Err(WriteError::Commit(e)) => match &*e {
            masstuffy_collection::CommitError::WrongGeneration { expected, got, .. } => {
                Err(Error::StaleLocator {
                    expected: *expected,
                    got: *got,
                })
            }
            _ => Err(Error::Commit {
                collection: id,
                source: e,
            }),
        },
        Err(WriteError::Reopen(source)) => Err(Error::Reopen {
            collection: id,
            source,
        }),
        Err(WriteError::Internal(s)) => Err(Error::Internal(s)),
    }
}
